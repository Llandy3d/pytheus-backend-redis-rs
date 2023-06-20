use itertools::Itertools;
use pyo3::exceptions::PyException;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use redis::Commands;
use redis::{Connection, RedisResult};
use std::collections::HashMap;
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;

// This could be completely wrong, not sure if it would break the channel, let's try 🤞
static REDIS_JOB_TX: OnceLock<Mutex<mpsc::Sender<RedisJob>>> = OnceLock::new();
const EXPIRE_KEY_SECONDS: usize = 3600;

#[derive(Debug)]
enum BackendAction {
    Inc,
    Dec,
    Set,
    Get,
}

#[derive(Debug)]
struct RedisJobResult {
    value: f64,
}

#[derive(Debug)]
struct RedisJob {
    action: BackendAction,
    key_name: String,
    labels_hash: Option<String>,
    value: f64,
    result_tx: Option<mpsc::Sender<RedisJobResult>>,
}

#[pyclass]
struct RedisBackend {
    #[pyo3(get)]
    config: Py<PyDict>,
    #[pyo3(get)]
    metric: Py<PyAny>,
    #[pyo3(get)]
    histogram_bucket: Option<String>,
    redis_job_tx: mpsc::Sender<RedisJob>,
    key_name: String,
    labels_hash: Option<String>,
}

fn create_redis_connection() -> RedisResult<Connection> {
    let client = redis::Client::open("redis://127.0.0.1/")?;
    let con = client.get_connection()?;
    Ok(con)
}

#[pymethods]
impl RedisBackend {
    #[new]
    fn new(config: &PyDict, metric: &PyAny, histogram_bucket: Option<String>) -> PyResult<Self> {
        // producer
        let redis_job_tx_mutex = REDIS_JOB_TX.get().unwrap();
        let redis_job_tx = redis_job_tx_mutex.lock().unwrap();
        let cloned_tx = redis_job_tx.clone();

        let py = metric.py();
        let collector = metric.getattr(intern!(metric.py(), "_collector"))?;

        let key_name: String = metric
            .getattr(intern!(py, "_collector"))?
            .getattr(intern!(py, "name"))?
            .extract()?;

        let mut default_labels: Option<HashMap<&str, &str>> = None;
        let mut metric_labels: Option<HashMap<&str, &str>> = None;

        let py_metric_labels = metric.getattr(intern!(py, "_labels"))?;
        if py_metric_labels.is_true()? {
            let labels: HashMap<&str, &str> = py_metric_labels.extract()?;
            metric_labels = Some(labels);
        }

        // default labels
        if collector
            .getattr(intern!(py, "_default_labels_count"))?
            .is_true()?
        {
            let labels: HashMap<&str, &str> = collector
                .getattr(intern!(py, "_default_labels"))?
                .extract()?;

            default_labels = Some(labels);
        }

        let to_hash = {
            if let Some(mut default_labels) = default_labels {
                if let Some(metric_labels) = metric_labels {
                    default_labels.extend(&metric_labels);
                }
                Some(default_labels)
            } else {
                metric_labels
            }
        };

        let labels_hash = to_hash.map(|labels| labels.values().sorted().join("-"));

        Ok(Self {
            config: config.into(),
            metric: metric.into(),
            histogram_bucket,
            redis_job_tx: cloned_tx,
            key_name,
            labels_hash,
        })
    }

    #[classmethod]
    fn _initialize(cls: &PyType, _config: &PyDict) -> PyResult<()> {
        println!("hello: {}", cls);

        let mut connection = match create_redis_connection() {
            Ok(connection) => connection,
            Err(e) => return Err(PyException::new_err(e.to_string())),
        };

        // producer / consumer
        let (tx, rx) = mpsc::channel();
        REDIS_JOB_TX.get_or_init(|| Mutex::new(tx));

        thread::spawn(move || {
            println!("In thread....");
            while let Ok(received) = rx.recv() {
                println!("Got: {:?}", received);
                match received.action {
                    BackendAction::Inc | BackendAction::Dec => {
                        match received.labels_hash {
                            Some(labels_hash) => connection
                                .hincr(&received.key_name, &labels_hash, received.value)
                                .unwrap(),
                            None => connection.incr(&received.key_name, received.value).unwrap(),
                        }
                        let _: () = connection
                            .expire(&received.key_name, EXPIRE_KEY_SECONDS)
                            .unwrap();
                    }
                    BackendAction::Set => {
                        match received.labels_hash {
                            Some(labels_hash) => connection
                                .hset(&received.key_name, &labels_hash, received.value)
                                .unwrap(),
                            None => connection.set(&received.key_name, received.value).unwrap(),
                        }
                        let _: () = connection
                            .expire(&received.key_name, EXPIRE_KEY_SECONDS)
                            .unwrap();
                    }
                    BackendAction::Get => {
                        let get_result: Result<f64, redis::RedisError> = match received.labels_hash
                        {
                            Some(labels_hash) => connection.hget(&received.key_name, &labels_hash),
                            None => connection.get(&received.key_name),
                        };
                        let value: f64 = match get_result {
                            Ok(value) => {
                                // TODO: most likely will need to queue these operations
                                // waiting on the expire call before returning the value is not
                                // good
                                let _: () = connection
                                    .expire(&received.key_name, EXPIRE_KEY_SECONDS)
                                    .unwrap();
                                value
                            }
                            Err(e) => {
                                if e.kind() == redis::ErrorKind::TypeError {
                                    // This would happen when there is no key so `nil` is returned
                                    // so we return the default 0.0 value
                                    0.0
                                } else {
                                    // TODO: will need to handle the panic
                                    panic!("{e:?}");
                                }
                            }
                        };

                        received
                            .result_tx
                            .unwrap()
                            .send(RedisJobResult { value })
                            .unwrap();
                    }
                }
            }
        });

        Ok(())
    }

    fn inc(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Inc,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(), // I wonder if only the String inside should be cloned into a new Some
                value,
                result_tx: None,
            })
            .unwrap();
    }

    fn dec(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Dec,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(),
                value: -value,
                result_tx: None,
            })
            .unwrap();
    }

    fn set(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Set,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(),
                value,
                result_tx: None,
            })
            .unwrap();
    }

    fn get(&self) -> f64 {
        let (tx, rx) = mpsc::channel();
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Get,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(),
                value: f64::NAN,
                result_tx: Some(tx),
            })
            .unwrap();

        // TODO: should free the GIL in here
        let job_result = rx.recv().unwrap();
        job_result.value
    }
}

#[pyclass]
struct SingleProcessBackend {
    #[pyo3(get)]
    config: Py<PyDict>,
    #[pyo3(get)]
    metric: Py<PyAny>,
    #[pyo3(get)]
    histogram_bucket: Option<String>,
    value: Mutex<f64>,
}

#[pymethods]
impl SingleProcessBackend {
    #[new]
    fn new(config: &PyDict, metric: &PyAny, histogram_bucket: Option<String>) -> Self {
        Self {
            config: config.into(),
            metric: metric.into(),
            histogram_bucket,
            value: Mutex::new(0.0),
        }
    }

    fn inc(&mut self, value: f64) {
        let mut data = self.value.lock().unwrap();
        *data += value;
    }

    fn dec(&mut self, value: f64) {
        let mut data = self.value.lock().unwrap();
        *data -= value;
    }

    fn set(&mut self, value: f64) {
        let mut data = self.value.lock().unwrap();
        *data = value;
    }

    fn get(&self) -> f64 {
        let data = self.value.lock().unwrap();
        *data
    }
}

/// A Python module implemented in Rust.
#[pymodule]
fn pytheus_backend_rs(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<RedisBackend>()?;
    m.add_class::<SingleProcessBackend>()?;
    Ok(())
}
