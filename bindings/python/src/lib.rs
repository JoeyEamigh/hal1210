#![deny(clippy::all)]

use std::sync::{Arc, Mutex};

use hal1210client_core::{init_tracing, BindingError, ClientHandle};
use once_cell::sync::Lazy;
use pyo3::{
  exceptions::{PyRuntimeError, PyTypeError},
  prelude::*,
  types::{PyAny, PyDict, PyModule, PyType},
  Bound,
};
use pyo3_async_runtimes::tokio::future_into_py;
use serde_json::Value;
use tokio::{
  runtime::{Builder, Runtime},
  sync::broadcast,
  task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

#[pyclass(name = "Hal1210Client")]
pub struct PyHal1210Client {
  inner: ClientHandle,
  listeners: Arc<Mutex<Vec<Arc<PyListenerHandle>>>>,
}

#[pymethods]
impl PyHal1210Client {
  #[classmethod]
  #[pyo3(name = "connect", signature = (/,**kwargs))]
  fn connect_py(
    _cls: &Bound<'_, PyType>,
    kwargs: Option<&Bound<'_, PyDict>>,
    py: Python<'_>,
  ) -> PyResult<Py<PyHal1210Client>> {
    let enable_tracing = kwargs
      .and_then(|dict| dict.get_item("enable_tracing").unwrap_or(None))
      .and_then(|obj| obj.is_truthy().ok());
    if enable_tracing.unwrap_or(false) {
      init_tracing();
      debug!("pyhal1210client: tracing initialized");
    }
    let inner = py
      .detach(|| async_runtime().block_on(ClientHandle::connect()))
      .map_err(map_py_err)?;
    debug!("pyhal1210client: connected to daemon");
    Py::new(
      py,
      PyHal1210Client {
        inner,
        listeners: Arc::new(Mutex::new(Vec::new())),
      },
    )
  }

  #[pyo3(text_signature = "($self, message)")]
  fn send<'py>(&self, py: Python<'py>, message: Bound<'py, PyAny>) -> PyResult<String> {
    let payload = python_to_value(py, &message)?;
    let id = self.inner.send_json(payload).map_err(map_py_err)?;
    trace!(%id, "pyhal1210client: dispatched message");
    Ok(id.to_string())
  }

  #[pyo3(text_signature = "($self)")]
  fn next_message<'py>(&self, py: Python<'py>) -> PyResult<Option<Py<PyAny>>> {
    let message = py
      .detach(|| async_runtime().block_on(self.inner.next_message_json()))
      .map_err(map_py_err)?;
    trace!("pyhal1210client: next_message returned");
    match message {
      Some(value) => json_to_py(py, value).map(Some),
      None => Ok(None),
    }
  }

  #[pyo3(text_signature = "($self)", name = "next_message_async")]
  fn next_message_async<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
    let handle = self.inner.clone();
    future_into_py(py, async move {
      let message = handle.next_message_json().await.map_err(map_py_err)?;
      trace!("pyhal1210client: next_message_async resolved");
      Python::attach(|gil| -> PyResult<Py<PyAny>> {
        match message {
          Some(value) => json_to_py(gil, value),
          None => Ok(gil.None()),
        }
      })
    })
  }

  #[pyo3(text_signature = "($self, callback)")]
  fn on_message<'py>(&self, py: Python<'py>, callback: Bound<'py, PyAny>) -> PyResult<()> {
    if !callback.is_callable() {
      return Err(PyTypeError::new_err("callback must be callable"));
    }

    let stored_callback = callback.unbind();
    let mut rx = self.inner.subscribe();
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    let listener_callback = stored_callback.clone_ref(py);

    let handle: JoinHandle<()> = async_runtime().spawn(async move {
      loop {
        tokio::select! {
          _ = stopper.cancelled() => break,
          result = rx.recv() => {
            match result {
              Ok(msg) => {
                match serde_json::to_value(msg) {
                  Ok(payload) => {
                    if let Err(err) = Python::attach(|gil| -> PyResult<()> {
                      trace!("pyhal1210client: invoking on_message callback");
                      let py_value = json_to_py(gil, payload)?;
                      let callback = listener_callback.bind(gil);
                      callback.call1((py_value,))?;
                      Ok(())
                    }) {
                      eprintln!("hal1210client: Python callback error: {err}");
                      break;
                    }
                  }
                  Err(err) => {
                    eprintln!("hal1210client: failed to serialize message: {err}");
                    break;
                  }
                }
              }
              Err(broadcast::error::RecvError::Closed) => break,
              Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
          }
        }
      }
    });

    let listener = Arc::new(PyListenerHandle::new(cancel, handle, stored_callback));
    let mut guard = self.listeners.lock().expect("listeners mutex poisoned");
    guard.push(listener);
    Ok(())
  }

  #[pyo3(text_signature = "($self)")]
  fn cancel(&self) {
    self.stop_all_listeners();
    self.inner.cancel();
    debug!("pyhal1210client: cancel requested");
  }
}

impl PyHal1210Client {
  fn stop_all_listeners(&self) {
    let listeners = {
      let mut guard = self.listeners.lock().expect("listeners mutex poisoned");
      guard.drain(..).collect::<Vec<_>>()
    };
    for listener in listeners {
      listener.stop();
    }
  }
}

impl Drop for PyHal1210Client {
  fn drop(&mut self) {
    self.stop_all_listeners();
  }
}

#[pymodule]
fn pyhal1210client(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
  m.add_class::<PyHal1210Client>()?;
  Ok(())
}

fn async_runtime() -> &'static Runtime {
  static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    Builder::new_multi_thread()
      .worker_threads(2)
      .enable_all()
      .thread_name("hal1210-client")
      .build()
      .expect("failed to build Tokio runtime")
  });
  &RUNTIME
}

fn python_json_module(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
  py.import("json")
}

fn python_to_value(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<Value> {
  let json = python_json_module(py)?;
  let dumps = json.getattr("dumps")?;
  let dumped: String = dumps.call1((obj,))?.extract()?;
  serde_json::from_str(&dumped).map_err(json_err)
}

fn json_to_py(py: Python<'_>, value: Value) -> PyResult<Py<PyAny>> {
  let payload = serde_json::to_string(&value).map_err(json_err)?;
  let json = python_json_module(py)?;
  let loads = json.getattr("loads")?;
  let obj = loads.call1((payload,))?;
  Ok(obj.unbind())
}

fn json_err(err: serde_json::Error) -> PyErr {
  PyRuntimeError::new_err(err.to_string())
}

fn map_py_err(err: BindingError) -> PyErr {
  PyRuntimeError::new_err(err.to_string())
}

struct PyListenerHandle {
  cancel: CancellationToken,
  handle: Mutex<Option<JoinHandle<()>>>,
  #[allow(dead_code)]
  callback: Py<PyAny>,
}

impl PyListenerHandle {
  fn new(cancel: CancellationToken, handle: JoinHandle<()>, callback: Py<PyAny>) -> Self {
    Self {
      cancel,
      handle: Mutex::new(Some(handle)),
      callback,
    }
  }

  fn stop(&self) {
    self.cancel.cancel();
    if let Some(handle) = self.handle.lock().expect("subscription mutex poisoned").take() {
      handle.abort();
    }
  }
}

impl Drop for PyListenerHandle {
  fn drop(&mut self) {
    self.cancel.cancel();
    if let Some(handle) = self.handle.get_mut().ok().and_then(|inner| inner.take()) {
      handle.abort();
    }
  }
}
