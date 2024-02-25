mod atomic;

use crossbeam::channel;
use log::{error, info};
use pyo3::exceptions::PyException;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyType};
use redis::{from_redis_value, ConnectionLike, FromRedisValue, RedisResult, Value};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;

// This could be completely wrong, not sure if it would break the channel, let's try 🤞
static REDIS_JOB_TX: OnceLock<Mutex<mpsc::Sender<RedisJob>>> = OnceLock::new();
static REDIS_PIPELINE_JOB_TX: OnceLock<Mutex<channel::Sender<RedisPipelineJob>>> = OnceLock::new();
const EXPIRE_KEY_SECONDS: usize = 3600;

#[derive(Debug)]
enum BackendAction {
    Inc,
    Dec,
    Set,
}

#[derive(Debug)]
struct RedisPipelineJobResult {
    values: Result<Vec<PipelineResult>, PyErr>,
}

#[derive(Debug)]
struct RedisJob {
    action: BackendAction,
    key_name: String,
    labels_hash: Option<String>,
    value: f64,
}

struct RedisPipelineJob {
    pipeline: redis::Pipeline,
    result_tx: mpsc::Sender<RedisPipelineJobResult>,
}

#[derive(Debug)]
#[pyclass]
struct RedisBackend {
    #[pyo3(get)]
    config: Py<PyDict>,
    #[pyo3(get)]
    metric: Py<PyAny>,
    #[pyo3(get)]
    histogram_bucket: Option<String>,
    redis_job_tx: mpsc::Sender<RedisJob>,
    #[pyo3(get)]
    key_name: String,
    #[pyo3(get)]
    labels_hash: Option<String>,
}

#[derive(Debug)]
#[pyclass]
struct OutSample {
    #[pyo3(get)]
    suffix: String,
    #[pyo3(get)]
    labels: Option<BTreeMap<String, String>>,
    #[pyo3(get)]
    value: f64,
}

impl OutSample {
    fn new(suffix: String, labels: Option<BTreeMap<String, String>>, value: f64) -> Self {
        Self {
            suffix,
            labels,
            value,
        }
    }
}

#[derive(Debug)]
struct SamplesResultDict {
    collectors: Vec<Py<PyAny>>,
    samples_vec: Vec<Vec<OutSample>>,
}

impl SamplesResultDict {
    fn new() -> Self {
        Self {
            collectors: vec![],
            samples_vec: vec![],
        }
    }
}

impl IntoPy<PyResult<PyObject>> for SamplesResultDict {
    fn into_py(self, py: Python<'_>) -> PyResult<PyObject> {
        let pydict = PyDict::new(py);
        for (collector, samples) in self
            .collectors
            .into_iter()
            .zip(self.samples_vec.into_iter())
        {
            pydict.set_item(collector, samples.into_py(py))?;
        }
        Ok(pydict.into())
    }
}

fn create_redis_pool(
    host: &str,
    port: u16,
) -> Result<r2d2::Pool<redis::Client>, Box<dyn std::error::Error>> {
    let url = format!("redis://{host}:{port}");
    let client = redis::Client::open(url)?;
    let pool = r2d2::Pool::builder().build(client)?;
    Ok(pool)
}

fn add_job_to_pipeline(received: RedisJob, pipe: &mut redis::Pipeline) {
    match received.action {
        BackendAction::Inc | BackendAction::Dec => {
            match received.labels_hash {
                Some(labels_hash) => pipe
                    .hincr(&received.key_name, &labels_hash, received.value)
                    .ignore(),
                None => pipe.incr(&received.key_name, received.value).ignore(),
            };
            pipe.expire(&received.key_name, EXPIRE_KEY_SECONDS).ignore();
        }
        BackendAction::Set => {
            match received.labels_hash {
                Some(labels_hash) => pipe
                    .hset(&received.key_name, &labels_hash, received.value)
                    .ignore(),
                None => pipe.set(&received.key_name, received.value).ignore(),
            };
            pipe.expire(&received.key_name, EXPIRE_KEY_SECONDS).ignore();
        }
    }
}

#[derive(Debug)]
enum PipelineResult {
    Float(f64),
    Hash(BTreeMap<String, String>),
}

impl FromRedisValue for PipelineResult {
    fn from_redis_value(v: &Value) -> RedisResult<Self> {
        let result = match v {
            Value::Bulk(_) => {
                let map: BTreeMap<String, String> = from_redis_value(v)?;
                PipelineResult::Hash(map)
            }
            _ => {
                let float: Option<f64> = from_redis_value(v)?;
                PipelineResult::Float(float.unwrap_or(0f64))
            }
        };

        Ok(result)
    }
}

fn handle_generate_metrics_job(
    pipeline: redis::Pipeline,
    connection: &mut r2d2::PooledConnection<redis::Client>,
    pool: &r2d2::Pool<redis::Client>,
) -> Result<Vec<PipelineResult>, Box<dyn std::error::Error>> {
    if !connection.is_open() {
        *connection = pool.get()?
    }

    let values: Vec<PipelineResult> = pipeline.query(connection)?;

    Ok(values)
}

fn handle_backend_action_job(
    received: RedisJob,
    connection: &mut r2d2::PooledConnection<redis::Client>,
    pool: &r2d2::Pool<redis::Client>,
    rx: &mpsc::Receiver<RedisJob>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !connection.is_open() {
        *connection = pool.get()?;
    }

    let mut pipe = redis::pipe();

    add_job_to_pipeline(received, &mut pipe);

    for received in rx.try_iter() {
        add_job_to_pipeline(received, &mut pipe);
    }

    pipe.query::<()>(connection)?;

    Ok(())
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

        let mut key_name: String = metric
            .getattr(intern!(py, "_collector"))?
            .getattr(intern!(py, "name"))?
            .extract()?;

        if let Some(bucket_id) = histogram_bucket.clone() {
            key_name = format!("{key_name}:{bucket_id}");
        }

        // BTreeMap is used to order by key so that the labels_hash will
        // always be sorted
        let mut default_labels: Option<BTreeMap<&str, &str>> = None;
        let mut metric_labels: Option<BTreeMap<&str, &str>> = None;

        let py_metric_labels = metric.getattr(intern!(py, "_labels"))?;
        if py_metric_labels.is_true()? {
            let labels: BTreeMap<&str, &str> = py_metric_labels.extract()?;
            metric_labels = Some(labels);
        }

        // default labels
        if collector
            .getattr(intern!(py, "_default_labels_count"))?
            .is_true()?
        {
            let labels: BTreeMap<&str, &str> = collector
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

        let labels_hash = {
            if let Some(labels) = to_hash {
                match serde_json::to_string(&labels) {
                    Ok(hash) => Some(hash),
                    Err(e) => return Err(PyException::new_err(e.to_string())),
                }
            } else {
                None
            }
        };

        let new_backend = Self {
            config: config.into(),
            metric: metric.into(),
            histogram_bucket,
            redis_job_tx: cloned_tx,
            key_name,
            labels_hash,
        };

        new_backend._initialize_key();
        Ok(new_backend)
    }

    #[classmethod]
    fn _initialize(_cls: &PyType, config: &PyDict) -> PyResult<()> {
        // using the PyAny::get_item so that it will raise a KeyError on missing key
        let host: &str = PyAny::get_item(config, intern!(config.py(), "host"))?.extract()?;
        let port: u16 = PyAny::get_item(config, intern!(config.py(), "port"))?.extract()?;

        let pool = match create_redis_pool(host, port) {
            Ok(pool) => pool,
            Err(e) => return Err(PyException::new_err(e.to_string())),
        };

        // producer / consumer
        let (tx, rx) = mpsc::channel();
        REDIS_JOB_TX.get_or_init(|| Mutex::new(tx));

        let (pipeline_tx, pipeline_rx) = crossbeam::channel::unbounded();
        REDIS_PIPELINE_JOB_TX.get_or_init(|| Mutex::new(pipeline_tx));

        for i in 0..4 {
            let cloned_pipeline_rx = pipeline_rx.clone();
            let pool = pool.clone();
            info!("Starting pipeline thread....{i}");
            thread::spawn(move || {
                // the first connection happens at startup so we let it panic
                let mut connection = pool.get().unwrap();
                while let Ok(received) = cloned_pipeline_rx.recv() {
                    let values =
                        handle_generate_metrics_job(received.pipeline, &mut connection, &pool);
                    let values = values.map_err(|e| PyException::new_err(e.to_string()));

                    // NOTE: might want to log the failure
                    let _ = received.result_tx.send(RedisPipelineJobResult { values });
                }
            });
        }

        info!("Starting BackendAction thread....");
        thread::spawn(move || loop {
            // the first connection happens at startup so we let it panic
            let mut connection = pool.get().unwrap();
            while let Ok(received) = rx.recv() {
                handle_backend_action_job(received, &mut connection, &pool, &rx)
                    .unwrap_or_else(|e| error!("{}", e.to_string()));
            }
        });

        info!("RedisBackend initialized");
        Ok(())
    }

    #[classmethod]
    fn _generate_samples(cls: &PyType, registry: &PyAny) -> PyResult<PyObject> {
        let py = cls.py();
        let collectors = registry.call_method0(intern!(py, "collect"))?;

        let metric_collectors: PyResult<Vec<&PyAny>> = collectors
            .iter()?
            .map(|i| i.and_then(PyAny::extract))
            .collect();

        let mut samples_result_dict = SamplesResultDict::new();

        let mut pipe = redis::pipe();

        // TODO: need to support custom collectors
        for metric_collector in metric_collectors? {
            let samples_list: Vec<OutSample> = vec![];

            samples_result_dict.collectors.push(metric_collector.into());
            samples_result_dict.samples_vec.push(samples_list);

            let key_name: &str = metric_collector.getattr(intern!(py, "name"))?.extract()?;

            let collector_type: &str = metric_collector.getattr(intern!(py, "type_"))?.extract()?;
            let has_labels: bool = metric_collector
                .getattr(intern!(py, "_required_labels"))?
                .is_true()?;

            match collector_type {
                "counter" | "gauge" => {
                    pipe.expire(key_name, EXPIRE_KEY_SECONDS).ignore();
                    if has_labels {
                        pipe.hgetall(key_name);
                    } else {
                        pipe.get(key_name);
                    }
                }
                "summary" => {
                    for suffix in ["count", "sum"] {
                        let key_with_suffix = format!("{}:{}", key_name, suffix);
                        pipe.expire(key_with_suffix.clone(), EXPIRE_KEY_SECONDS)
                            .ignore();
                        if has_labels {
                            pipe.hgetall(key_with_suffix);
                        } else {
                            pipe.get(key_with_suffix.clone());
                        }
                    }
                }
                "histogram" => {
                    let extra_suffixes = ["+Inf", "count", "sum"];
                    let upper_bounds: Vec<f64> = metric_collector
                        .getattr(intern!(py, "_metric"))?
                        .getattr(intern!(py, "_upper_bounds"))?
                        .extract()?;
                    let upper_bounds = &upper_bounds[..upper_bounds.len() - 1]; // remove inf
                    let upper_bounds: Vec<String> =
                        upper_bounds.iter().map(|bound| bound.to_string()).collect();

                    let suffixes = upper_bounds
                        .iter()
                        .map(|bound| bound.as_str())
                        .chain(extra_suffixes);

                    for suffix in suffixes {
                        let key_with_suffix = format!("{}:{}", key_name, suffix);
                        pipe.expire(key_with_suffix.clone(), EXPIRE_KEY_SECONDS)
                            .ignore();
                        if has_labels {
                            pipe.hgetall(key_with_suffix);
                        } else {
                            pipe.get(key_with_suffix.clone());
                        }
                    }
                }
                _ => (),
            }
        }

        let send_tx = {
            let redis_pipeline_job_tx_job_tx_mutex = REDIS_PIPELINE_JOB_TX.get().unwrap();
            let redis_pipeline_job_tx = redis_pipeline_job_tx_job_tx_mutex.lock().unwrap();
            redis_pipeline_job_tx.clone()
        };

        let (tx, rx) = mpsc::channel();

        send_tx
            .send(RedisPipelineJob {
                result_tx: tx,
                pipeline: pipe,
            })
            .unwrap();

        let job_result = py.allow_threads(move || rx.recv().unwrap());
        let values = job_result.values?;
        let mut values_iterator = values.iter();

        for (collector, samples_list) in samples_result_dict
            .collectors
            .iter_mut()
            .zip(samples_result_dict.samples_vec.iter_mut())
        {
            let collector_type: String =
                collector.getattr(py, intern!(py, "type_"))?.extract(py)?;

            let mut current_value = values_iterator.next().unwrap();

            match collector_type.as_str() {
                "counter" | "gauge" => match current_value {
                    PipelineResult::Float(float) => {
                        let out_sample = OutSample::new("".to_string(), None, *float);
                        samples_list.push(out_sample);
                    }
                    PipelineResult::Hash(hash) => {
                        for (labels, value) in hash {
                            let labels_map: BTreeMap<String, String> = {
                                match serde_json::from_str(labels) {
                                    Ok(map) => map,
                                    Err(e) => return Err(PyException::new_err(e.to_string())),
                                }
                            };
                            let out_sample = OutSample::new(
                                "".to_string(),
                                Some(labels_map),
                                value.parse::<f64>().unwrap(),
                            );
                            samples_list.push(out_sample);
                        }
                    }
                },
                "summary" => match current_value {
                    PipelineResult::Float(float) => {
                        let count_value = float;
                        current_value = values_iterator.next().unwrap();
                        let sum_value = {
                            if let PipelineResult::Float(float) = current_value {
                                float
                            } else {
                                return Err(PyException::new_err(
                                            "Critical library error while building metrics. Expected float found hash",
                                        ));
                            }
                        };

                        let count_sample = OutSample::new("_count".to_string(), None, *count_value);
                        let sum_sample = OutSample::new("_sum".to_string(), None, *sum_value);
                        samples_list.push(count_sample);
                        samples_list.push(sum_sample);
                    }

                    PipelineResult::Hash(hash) => {
                        let mut ordered_samples = BTreeMap::new();
                        let count_hash = hash;
                        current_value = values_iterator.next().unwrap();
                        let sum_hash = {
                            if let PipelineResult::Hash(map) = current_value {
                                map
                            } else {
                                return Err(PyException::new_err(
                                    "Critical library error while building metrics. Expected hash",
                                ));
                            }
                        };

                        for (labels, value) in count_hash {
                            let labels_map: BTreeMap<String, String> = {
                                match serde_json::from_str(labels) {
                                    Ok(map) => map,
                                    Err(e) => return Err(PyException::new_err(e.to_string())),
                                }
                            };
                            let out_sample = OutSample::new(
                                "_count".to_string(),
                                Some(labels_map),
                                value.parse::<f64>().unwrap(),
                            );
                            ordered_samples
                                .entry(labels)
                                .or_insert(vec![])
                                .push(out_sample);
                        }

                        for (labels, value) in sum_hash {
                            let labels_map: BTreeMap<String, String> = {
                                match serde_json::from_str(labels) {
                                    Ok(map) => map,
                                    Err(e) => return Err(PyException::new_err(e.to_string())),
                                }
                            };
                            let out_sample = OutSample::new(
                                "_sum".to_string(),
                                Some(labels_map),
                                value.parse::<f64>().unwrap(),
                            );
                            ordered_samples
                                .entry(labels)
                                .or_insert(vec![])
                                .push(out_sample);
                        }

                        for ordered_samples_list in ordered_samples.values_mut() {
                            samples_list.append(ordered_samples_list);
                        }
                    }
                },
                "histogram" => match current_value {
                    PipelineResult::Float(float) => {
                        let mut first_iteration = true;
                        let extra_suffixes = ["+Inf", "count", "sum"];
                        let upper_bounds: Vec<f64> = collector
                            .getattr(py, intern!(py, "_metric"))?
                            .getattr(py, intern!(py, "_upper_bounds"))?
                            .extract(py)?;
                        let upper_bounds = &upper_bounds[..upper_bounds.len() - 1]; // remove inf
                        let upper_bounds: Vec<String> =
                            upper_bounds.iter().map(|bound| bound.to_string()).collect();

                        let suffixes = upper_bounds
                            .iter()
                            .map(|bound| bound.as_str())
                            .chain(extra_suffixes);

                        for suffix in suffixes {
                            let mut float = float;
                            if !first_iteration {
                                current_value = values_iterator.next().unwrap();
                                float = {
                                    if let PipelineResult::Float(float) = current_value {
                                        float
                                    } else {
                                        return Err(PyException::new_err(
                                            "Critical library error while building metrics. Expected float found hash",
                                        ));
                                    }
                                };
                            } else {
                                first_iteration = false;
                            }
                            match suffix {
                                "count" => {
                                    let out_sample =
                                        OutSample::new("_count".to_string(), None, *float);
                                    samples_list.push(out_sample);
                                }
                                "sum" => {
                                    let out_sample =
                                        OutSample::new("_sum".to_string(), None, *float);
                                    samples_list.push(out_sample);
                                }
                                _ => {
                                    let mut labels_map = BTreeMap::new();
                                    labels_map.insert("le".to_string(), suffix.to_string());
                                    let out_sample = OutSample::new(
                                        "_bucket".to_string(),
                                        Some(labels_map),
                                        *float,
                                    );
                                    samples_list.push(out_sample);
                                }
                            }
                        }
                    }
                    PipelineResult::Hash(hash) => {
                        let mut first_iteration = true;
                        let extra_suffixes = ["+Inf", "count", "sum"];
                        let upper_bounds: Vec<f64> = collector
                            .getattr(py, intern!(py, "_metric"))?
                            .getattr(py, intern!(py, "_upper_bounds"))?
                            .extract(py)?;
                        let upper_bounds = &upper_bounds[..upper_bounds.len() - 1]; // remove inf
                        let upper_bounds: Vec<String> =
                            upper_bounds.iter().map(|bound| bound.to_string()).collect();

                        let suffixes = upper_bounds
                            .iter()
                            .map(|bound| bound.as_str())
                            .chain(extra_suffixes);

                        let mut ordered_samples = BTreeMap::new();

                        for suffix in suffixes {
                            let mut hash = hash;
                            if !first_iteration {
                                current_value = values_iterator.next().unwrap();
                                hash = {
                                    if let PipelineResult::Hash(map) = current_value {
                                        map
                                    } else {
                                        return Err(PyException::new_err(
                                            "Critical library error while building metrics. Expected hash",
                                        ));
                                    }
                                };
                            } else {
                                first_iteration = false;
                            }
                            match suffix {
                                "count" => {
                                    for (labels, value) in hash {
                                        let labels_map: BTreeMap<String, String> = {
                                            match serde_json::from_str(labels) {
                                                Ok(map) => map,
                                                Err(e) => {
                                                    return Err(PyException::new_err(e.to_string()))
                                                }
                                            }
                                        };
                                        let out_sample = OutSample::new(
                                            "_count".to_string(),
                                            Some(labels_map),
                                            value.parse::<f64>().unwrap(),
                                        );
                                        ordered_samples
                                            .entry(labels)
                                            .or_insert(vec![])
                                            .push(out_sample);
                                    }
                                }
                                "sum" => {
                                    for (labels, value) in hash {
                                        let labels_map: BTreeMap<String, String> = {
                                            match serde_json::from_str(labels) {
                                                Ok(map) => map,
                                                Err(e) => {
                                                    return Err(PyException::new_err(e.to_string()))
                                                }
                                            }
                                        };
                                        let out_sample = OutSample::new(
                                            "_sum".to_string(),
                                            Some(labels_map),
                                            value.parse::<f64>().unwrap(),
                                        );
                                        ordered_samples
                                            .entry(labels)
                                            .or_insert(vec![])
                                            .push(out_sample);
                                    }
                                }
                                _ => {
                                    for (labels, value) in hash {
                                        let mut labels_map: BTreeMap<String, String> = {
                                            match serde_json::from_str(labels) {
                                                Ok(map) => map,
                                                Err(e) => {
                                                    return Err(PyException::new_err(e.to_string()))
                                                }
                                            }
                                        };
                                        labels_map.insert("le".to_string(), suffix.to_string());
                                        let out_sample = OutSample::new(
                                            "_bucket".to_string(),
                                            Some(labels_map),
                                            value.parse::<f64>().unwrap(),
                                        );
                                        ordered_samples
                                            .entry(labels)
                                            .or_insert(vec![])
                                            .push(out_sample);
                                    }
                                }
                            }
                        }
                        for ordered_samples_list in ordered_samples.values_mut() {
                            samples_list.append(ordered_samples_list);
                        }
                    }
                },
                _ => (),
            }
        }

        samples_result_dict.into_py(py)
    }

    fn _initialize_key(&self) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Inc,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(), // I wonder if only the String inside should be cloned into a new Some
                value: 0.0,
            })
            .unwrap_or_else(|_| error!("`_initialize_key` operation failed"));
    }

    fn inc(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Inc,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(), // I wonder if only the String inside should be cloned into a new Some
                value,
            })
            .unwrap_or_else(|_| error!("`inc` operation failed"));
    }

    fn dec(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Dec,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(),
                value: -value,
            })
            .unwrap_or_else(|_| error!("`dec` operation failed"));
    }

    fn set(&self, value: f64) {
        self.redis_job_tx
            .send(RedisJob {
                action: BackendAction::Set,
                key_name: self.key_name.clone(),
                labels_hash: self.labels_hash.clone(),
                value,
            })
            .unwrap_or_else(|_| error!("`set` operation failed"));
    }

    fn get(&self) -> f64 {
        // This returns the float 0.0 because it's only called when an existing collector is not
        // able to find the data in the cache, meaning that it was not initialized yet.
        0.0
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

#[pyclass]
struct SingleProcessAtomicBackend {
    #[pyo3(get)]
    config: Py<PyDict>,
    #[pyo3(get)]
    metric: Py<PyAny>,
    #[pyo3(get)]
    histogram_bucket: Option<String>,
    value: atomic::AtomicF64,
}

#[pymethods]
impl SingleProcessAtomicBackend {
    #[new]
    fn new(config: &PyDict, metric: &PyAny, histogram_bucket: Option<String>) -> Self {
        Self {
            config: config.into(),
            metric: metric.into(),
            histogram_bucket,
            value: atomic::AtomicF64::new(0.0),
        }
    }

    fn inc(&mut self, value: f64) {
        self.value.fetch_add(value, Ordering::Relaxed);
    }

    fn dec(&mut self, value: f64) {
        self.value.fetch_sub(value, Ordering::Relaxed);
    }

    fn set(&mut self, value: f64) {
        self.value.store(value, Ordering::Relaxed);
    }

    fn get(&self) -> f64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// A Python module implemented in Rust.
#[pymodule]
fn pytheus_backend_rs(_py: Python, m: &PyModule) -> PyResult<()> {
    pyo3_log::init();

    m.add_class::<RedisBackend>()?;
    m.add_class::<SingleProcessBackend>()?;
    m.add_class::<SingleProcessAtomicBackend>()?;
    m.add_class::<OutSample>()?;
    Ok(())
}
