use std::cell::RefCell;
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::orchestrator::ResultWrapper;
use crate::transport::MapCubeErrExt;
use crate::utils::bind_method;
use async_trait::async_trait;
use cubesql::transport::{SqlGenerator, SqlTemplates};
use cubesql::CubeError;
#[cfg(debug_assertions)]
use log::trace;
use neon::prelude::*;
use tokio::sync::oneshot;

type JsAsyncStringChannelCallback =
    Box<dyn FnOnce(Result<String, CubeError>) -> Result<(), CubeError> + Send>;
type JsAsyncChannelCallback = Box<
    dyn FnOnce(&mut FunctionContext, Result<Handle<JsValue>, CubeError>) -> Result<(), CubeError>
        + Send,
>;

#[cfg(debug_assertions)]
static JS_ASYNC_CHANNEL_DEBUG_ID_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct JsAsyncChannel {
    #[cfg(debug_assertions)]
    _id: u64,
    callback: Option<JsAsyncChannelCallback>,
}

type BoxedChannel = JsBox<RefCell<JsAsyncChannel>>;

impl Finalize for JsAsyncChannel {}

fn js_async_channel_resolve(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let this = cx
        .this::<BoxedChannel>()?
        .downcast_or_throw::<BoxedChannel, _>(&mut cx)?;

    #[cfg(debug_assertions)]
    trace!("JsAsyncChannel.resolved {}", this.borrow()._id);

    let result = cx.argument::<JsValue>(0)?;

    let tricky_rust_scope_hack = if let Err(err) = this.borrow_mut().resolve(&mut cx, result) {
        cx.throw_error(format!("JsAsyncChannel resolving error: {}", err))
    } else {
        Ok(cx.undefined())
    };

    tricky_rust_scope_hack
}

fn js_async_channel_reject(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let this = cx
        .this::<BoxedChannel>()?
        .downcast_or_throw::<BoxedChannel, _>(&mut cx)?;

    #[cfg(debug_assertions)]
    trace!("JsAsyncChannel.reject {}", this.borrow()._id);

    let error = cx.argument::<JsString>(0)?;

    let error_str = error.value(&mut cx);

    let tricky_rust_scope_hack = if let Err(err) = this.borrow_mut().reject(&mut cx, error_str) {
        cx.throw_error(format!("JsAsyncChannel rejecting error: {}", err))
    } else {
        Ok(cx.undefined())
    };

    tricky_rust_scope_hack
}

impl JsAsyncChannel {
    pub fn new(callback: JsAsyncStringChannelCallback) -> Self {
        Self::new_raw(Box::new(move |cx, res| {
            callback(
                res.and_then(|v| {
                    v.downcast::<JsString, _>(cx).map_err(|e| {
                        CubeError::internal(format!("Can't downcast callback argument: {}", e))
                    })
                })
                .map(|v| v.value(cx)),
            )
        }))
    }

    pub fn new_raw(callback: JsAsyncChannelCallback) -> Self {
        Self {
            #[cfg(debug_assertions)]
            _id: JS_ASYNC_CHANNEL_DEBUG_ID_SEQ.fetch_add(1, Ordering::SeqCst),
            callback: Some(callback),
        }
    }

    #[allow(clippy::wrong_self_convention)]
    fn to_object<'a, C: Context<'a>>(self, cx: &mut C) -> JsResult<'a, JsObject> {
        let obj = cx.empty_object();
        // Pass JsAsyncChannel as this, because JsFunction cannot use closure (fn with move)
        let obj_this = cx.boxed(RefCell::new(self)).upcast::<JsValue>();

        let resolve_fn = JsFunction::new(cx, js_async_channel_resolve)?;
        let resolve = bind_method(cx, resolve_fn, obj_this)?;
        obj.set(cx, "resolve", resolve)?;

        let reject_fn = JsFunction::new(cx, js_async_channel_reject)?;
        let reject = bind_method(cx, reject_fn, obj_this)?;
        obj.set(cx, "reject", reject)?;

        Ok(obj)
    }

    fn resolve(
        &mut self,
        cx: &mut FunctionContext,
        result: Handle<JsValue>,
    ) -> Result<(), CubeError> {
        if let Some(callback) = self.callback.take() {
            callback(cx, Ok(result))
        } else {
            Err(CubeError::internal(
                "Resolve was called on AsyncChannel that was already used".to_string(),
            ))
        }
    }

    fn reject(&mut self, cx: &mut FunctionContext, error: String) -> Result<(), CubeError> {
        if let Some(callback) = self.callback.take() {
            callback(cx, Err(CubeError::internal(error)))
        } else {
            Err(CubeError::internal(
                "Reject was called on AsyncChannel that was already used".to_string(),
            ))
        }
    }
}

pub async fn call_js_with_channel_as_callback<R>(
    channel: Arc<Channel>,
    js_method: Arc<Root<JsFunction>>,
    query: Option<String>,
) -> Result<R, CubeError>
where
    R: 'static + serde::de::DeserializeOwned + Send + std::fmt::Debug,
{
    let (tx, rx) = oneshot::channel::<Result<R, CubeError>>();

    let async_channel = JsAsyncChannel::new(Box::new(move |result| {
        let to_channel = match result {
            // @todo Optimize? Into?
            Ok(buffer_as_str) => match serde_json::from_str::<R>(&buffer_as_str) {
                Ok(json) => Ok(json),
                Err(err) => Err(CubeError::internal(err.to_string())),
            },
            Err(err) => Err(CubeError::internal(err.to_string())),
        };

        if tx.send(to_channel).is_err() {
            log::debug!("AsyncChannel: Unable to send result from JS back to Rust, channel closed");
        }
        Ok(())
    }));

    channel
        .try_send(move |mut cx| {
            // https://github.com/neon-bindings/neon/issues/672
            let method = match Arc::try_unwrap(js_method) {
                Ok(v) => v.into_inner(&mut cx),
                Err(v) => v.as_ref().to_inner(&mut cx),
            };

            let this = cx.undefined();
            let args: Vec<Handle<JsValue>> = vec![
                if let Some(q) = query {
                    cx.string(q).upcast::<JsValue>()
                } else {
                    cx.null().upcast::<JsValue>()
                },
                async_channel.to_object(&mut cx)?.upcast::<JsValue>(),
            ];

            method.call(&mut cx, this, args)?;

            Ok(())
        })
        .map_err(|err| {
            CubeError::internal(format!("Unable to send js call via channel, err: {}", err))
        })?;

    rx.await?
}

#[derive(Debug)]
pub enum ValueFromJs {
    String(String),
    ResultWrapper(Vec<ResultWrapper>),
}

#[allow(clippy::type_complexity)]
pub async fn call_raw_js_with_channel_as_callback<T, R>(
    channel: Arc<Channel>,
    js_method: Arc<Root<JsFunction>>,
    argument: T,
    arg_to_js_value: Box<
        dyn for<'a> FnOnce(&mut TaskContext<'a>, T) -> NeonResult<Handle<'a, JsValue>> + Send,
    >,
    result_from_js_value: Box<
        dyn FnOnce(&mut FunctionContext, Handle<JsValue>) -> Result<R, CubeError> + Send,
    >,
) -> Result<R, CubeError>
where
    R: 'static + Send + std::fmt::Debug,
    T: 'static + Send,
{
    let (tx, rx) = oneshot::channel::<Result<R, CubeError>>();

    let async_channel = JsAsyncChannel::new_raw(Box::new(move |cx, result| {
        let to_channel = result.and_then(|res| result_from_js_value(cx, res));

        tx.send(to_channel).map_err(|_| {
            CubeError::internal(
                "AsyncChannel: Unable to send result from JS back to Rust, channel closed"
                    .to_string(),
            )
        })
    }));

    channel.send(move |mut cx| {
        // https://github.com/neon-bindings/neon/issues/672
        let method = match Arc::try_unwrap(js_method) {
            Ok(v) => v.into_inner(&mut cx),
            Err(v) => v.as_ref().to_inner(&mut cx),
        };

        let this = cx.undefined();
        let arg_js_value = arg_to_js_value(&mut cx, argument)?;
        let args: Vec<Handle<JsValue>> = vec![
            arg_js_value,
            async_channel.to_object(&mut cx)?.upcast::<JsValue>(),
        ];

        method.call(&mut cx, this, args)?;

        Ok(())
    });

    rx.await?
}

type ArgsCallback =
    Box<dyn for<'a> FnOnce(&mut TaskContext<'a>) -> NeonResult<Vec<Handle<'a, JsValue>>> + Send>;

type ResultFromJsValue<R> =
    Box<dyn for<'a> FnOnce(&mut TaskContext<'a>, Handle<JsValue>) -> Result<R, CubeError> + Send>;

pub async fn call_js_fn<R: Sized + Send + 'static>(
    channel: Arc<Channel>,
    js_fn: Arc<Root<JsFunction>>,
    args_callback: ArgsCallback,
    result_from_js_value: ResultFromJsValue<R>,
    this: Arc<Root<JsObject>>,
) -> Result<R, CubeError> {
    let (tx, rx) = oneshot::channel::<Result<R, CubeError>>();

    channel
        .try_send(move |mut cx| {
            // https://github.com/neon-bindings/neon/issues/672
            let method = match Arc::try_unwrap(js_fn) {
                Ok(v) => v.into_inner(&mut cx),
                Err(v) => v.as_ref().to_inner(&mut cx),
            };
            let this = match Arc::try_unwrap(this) {
                Ok(v) => v.into_inner(&mut cx),
                Err(v) => v.as_ref().to_inner(&mut cx),
            };

            let args: Vec<Handle<JsValue>> = args_callback(&mut cx)?;

            let result = match method.call(&mut cx, this, args) {
                Ok(v) => v,
                Err(err) => {
                    println!("Unable to call js function: {}", err);
                    return Ok(());
                }
            };

            if tx.send(result_from_js_value(&mut cx, result)).is_err() {
                log::debug!(
                    "AsyncChannel: Unable to send result from JS back to Rust, channel closed"
                )
            }

            Ok(())
        })
        .map_err(|err| {
            CubeError::internal(format!("Unable to send js call via channel, err: {}", err))
        })?;

    rx.await?
}

#[derive(Debug)]
pub struct NodeSqlGenerator {
    channel: Arc<Channel>,
    sql_generator_obj: Option<Arc<Root<JsObject>>>,
    sql_templates: Arc<SqlTemplates>,
}

impl NodeSqlGenerator {
    pub fn new(
        cx: &mut FunctionContext,
        channel: Arc<Channel>,
        sql_generator_obj: Arc<Root<JsObject>>,
    ) -> Result<Self, CubeError> {
        let sql_templates = Arc::new(get_sql_templates(cx, sql_generator_obj.clone())?);
        Ok(NodeSqlGenerator {
            channel,
            sql_generator_obj: Some(sql_generator_obj),
            sql_templates,
        })
    }
}

fn get_sql_templates(
    cx: &mut FunctionContext,
    sql_generator: Arc<Root<JsObject>>,
) -> Result<SqlTemplates, CubeError> {
    let sql_generator = sql_generator.to_inner(cx);
    let reuse_params = sql_generator
        .get::<JsBoolean, _, _>(cx, "shouldReuseParams")
        .map_cube_err("Can't get shouldReuseParams")?
        .value(cx);
    let sql_templates = sql_generator
        .get::<JsFunction, _, _>(cx, "sqlTemplates")
        .map_cube_err("Can't get sqlTemplates")?;
    let templates = sql_templates
        .call(cx, sql_generator, Vec::new())
        .map_cube_err("Can't call sqlTemplates function")?
        .downcast_or_throw::<JsObject, _>(cx)
        .map_cube_err("Can't cast sqlTemplates to object")?;

    let template_types = templates
        .get_own_property_names(cx)
        .map_cube_err("Can't get template types")?;

    let mut templates_map = HashMap::new();

    for i in 0..template_types.len(cx) {
        let template_type = template_types
            .get::<JsString, _, _>(cx, i)
            .map_cube_err("Can't get template type")?;
        let template = templates
            .get::<JsObject, _, _>(cx, template_type)
            .map_cube_err("Can't get template")?;

        let template_names = template
            .get_own_property_names(cx)
            .map_cube_err("Can't get template names")?;

        for i in 0..template_names.len(cx) {
            let template_name = template_names
                .get::<JsString, _, _>(cx, i)
                .map_cube_err("Can't get function names")?;
            templates_map.insert(
                format!("{}/{}", template_type.value(cx), template_name.value(cx)),
                template
                    .get::<JsString, _, _>(cx, template_name)
                    .map_cube_err("Can't get function value")?
                    .value(cx),
            );
        }
    }

    SqlTemplates::new(templates_map, reuse_params)
}

// TODO impl drop for SqlGenerator
#[async_trait]
impl SqlGenerator for NodeSqlGenerator {
    fn get_sql_templates(&self) -> Arc<SqlTemplates> {
        self.sql_templates.clone()
    }

    #[allow(clippy::diverging_sub_expression)]
    async fn call_template(
        &self,
        _name: String,
        _params: HashMap<String, String>,
    ) -> Result<String, CubeError> {
        todo!()
    }
}

impl Drop for NodeSqlGenerator {
    fn drop(&mut self) {
        let channel = self.channel.clone();
        // Safety: Safe, because on_track take is used only for dropping
        let sql_generator_obj = self.sql_generator_obj.take().expect("Unable to take sql_generator_object while dropping NodeSqlGenerator, it was already taken");

        channel.send(move |mut cx| {
            let _ = match Arc::try_unwrap(sql_generator_obj) {
                Ok(v) => v.into_inner(&mut cx),
                Err(_) => {
                    log::error!("Unable to drop sql generator: reference is copied somewhere else. Potential memory leak");
                    return Ok(());
                },
            };
            Ok(())
        });
    }
}
