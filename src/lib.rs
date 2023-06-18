use pyo3::prelude::*;
use pyo3::types::PyDict;

#[pyclass]
struct RedisBackend {
    #[pyo3(get)]
    config: Py<PyDict>,
    #[pyo3(get)]
    metric: Py<PyAny>,
    #[pyo3(get)]
    histogram_bucket: Option<String>,
    value: f64,
}

#[pymethods]
impl RedisBackend {
    #[new]
    fn new(config: &PyDict, metric: &PyAny, histogram_bucket: Option<String>) -> Self {
        Self {
            config: config.into(),
            metric: metric.into(),
            histogram_bucket,
            value: 0.0,
        }
    }

    fn inc(&mut self, value: f64) {
        self.value += value
    }

    fn dec(&mut self, value: f64) {
        self.value -= value
    }

    fn set(&mut self, value: f64) {
        self.value = value
    }

    fn get(&self) -> f64 {
        self.value
    }
}

/// A Python module implemented in Rust.
#[pymodule]
fn pytheus_backend_redis_rs(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<RedisBackend>()?;
    Ok(())
}
