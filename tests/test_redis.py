import time
import pytest

from concurrent.futures import ProcessPoolExecutor
from pytheus.backends import load_backend
from pytheus.metrics import Counter, Histogram, Gauge, Summary, Sample
from pytheus.registry import CollectorRegistry
from pytheus_backend_rs import RedisBackend
from pytheus.exposition import generate_metrics


# when running a single test might not be picking up the backend so
# we enforce it
@pytest.fixture(autouse=True)
def load_redis_backend():
    load_backend(RedisBackend, {"host": "localhost", "port": 6379})


import redis

redis_client = redis.Redis(**{"host": "localhost", "port": 6379}, decode_responses=True)

# automatically clear the cache after every test function
@pytest.fixture(autouse=True)
# def clear_redis(redis_client):
def clear_redis():
    redis_client.flushall()


def test_smoke():
    load_backend(RedisBackend, {"host": "localhost", "port": 6379})
    counter = Counter("smoke", "smoke")
    counter.inc()
    time.sleep(0.01)
    assert redis_client.get('smoke') == '1'


def test_create_backend():
    counter = Counter("name", "desc")
    backend = RedisBackend({}, counter)

    assert backend.key_name == counter.name
    assert backend.histogram_bucket is None
    assert backend.labels_hash is None


def test_create_backend_labeled():
    counter = Counter("name", "desc", required_labels=["bob"])
    counter = counter.labels({"bob": "cat"})
    backend = RedisBackend({}, counter)

    assert backend.key_name == counter.name
    assert backend.histogram_bucket is None
    assert backend.labels_hash == '{"bob":"cat"}'


def test_create_backend_labeled_with_default():
    counter = Counter("name", "desc", required_labels=["bob"], default_labels={"bob": "cat"})
    backend = RedisBackend({}, counter)

    assert backend.key_name == counter.name
    assert backend.histogram_bucket is None
    assert backend.labels_hash == '{"bob":"cat"}'


def test_create_backend_labeled_with_default_mixed():
    counter = Counter(
        "name", "desc", required_labels=["bob", "bobby"], default_labels={"bob": "cat"}
    )
    counter = counter.labels({"bobby": "fish"})
    backend = RedisBackend({}, counter)

    assert backend.key_name == counter.name
    assert backend.histogram_bucket is None
    assert backend.labels_hash == '{"bob":"cat","bobby":"fish"}'


def test_create_backend_with_histogram_bucket():
    histogram_bucket = "+Inf"
    counter = Counter("name", "desc")
    backend = RedisBackend({}, counter, histogram_bucket=histogram_bucket)

    assert backend.key_name == f"{counter.name}:{histogram_bucket}"
    assert backend.histogram_bucket == histogram_bucket
    assert backend.labels_hash is None


def test_multiple_metrics_with_same_name_with_redis_overlap():
    """
    If sharing the same database, single value metrics will be overlapping.
    """
    first_collector = CollectorRegistry()
    second_collector = CollectorRegistry()

    counter_a = Counter("shared_name", "description", registry=first_collector)
    counter_b = Counter("shared_name", "description", registry=second_collector)

    counter_a.inc()

    time.sleep(0.01)
    assert redis_client.get('shared_name') == '1'


def test_multiple_metrics_with_same_name_labeled_with_redis_do_not_overlap():
    """
    Even while sharing the same database, labeled metrics won't be returned from collectors not
    having the specific child instance.
    """
    first_collector = CollectorRegistry()
    second_collector = CollectorRegistry()

    counter_a = Counter(
        "shared_name", "description", required_labels=["bob"], registry=first_collector
    )
    counter_b = Counter(
        "shared_name", "description", required_labels=["bob"], registry=second_collector
    )

    counter_a.labels({"bob": "cat"})
    counter_b.labels({"bob": "bobby"})

    first_collector_metrics_count = len(list(first_collector.collect().__next__().collect()))
    second_collector_metrics_count = len(list(second_collector.collect().__next__().collect()))

    assert first_collector_metrics_count == 1
    assert second_collector_metrics_count == 1


def test_multiple_metrics_with_same_name_labeled_with_redis_do_overlap_on_shared_child():
    """
    If sharing the same database, labeled metrics will be returned from collectors if having the
    same child instance.
    """
    first_collector = CollectorRegistry()
    second_collector = CollectorRegistry()

    counter_a = Counter(
        "shared_name", "description", required_labels=["bob"], registry=first_collector
    )
    counter_b = Counter(
        "shared_name", "description", required_labels=["bob"], registry=second_collector
    )

    counter_a.labels({"bob": "cat"})
    counter_b.labels({"bob": "bobby"})
    counter_b.labels({"bob": "cat"}).inc()

    first_collector_metrics_count = len(list(first_collector.collect().__next__().collect()))
    second_collector_metrics_count = len(list(second_collector.collect().__next__().collect()))

    assert first_collector_metrics_count == 1
    assert second_collector_metrics_count == 2
    time.sleep(0.01)
    backend_a = counter_a.labels({"bob": "cat"})._metric_value_backend
    backend_b = counter_b.labels({"bob": "cat"})._metric_value_backend
    assert redis_client.hget(backend_a.key_name, backend_a.labels_hash) == "1"
    assert redis_client.hget(backend_b.key_name, backend_b.labels_hash) == "1"


def test_generate_samples():
    registry = CollectorRegistry()
    counter = Counter("name", "desc", registry=registry)
    histogram = Histogram("histogram", "desc", registry=registry)
    samples = RedisBackend._generate_samples(registry)
    assert len(samples[counter._collector]) == 1
    assert len(samples[histogram._collector]) == 14


def test_generate_samples_with_labels():
    registry = CollectorRegistry()
    counter = Counter(
        "name", "desc", required_labels=["bob"], default_labels={"bob": "c"}, registry=registry
    )
    counter.labels({"bob": "a"})
    counter.labels({"bob": "b"})
    time.sleep(0.1)
    samples = RedisBackend._generate_samples(registry)
    assert len(samples[counter._collector]) == 3


def _run_multiprocess(extra_label):
    load_backend(
        backend_class=RedisBackend,
        backend_config={"host": "127.0.0.1", "port": 6379},
    )
    registry = CollectorRegistry()
    counter = Counter("name_multiple", "desc", required_labels=["bob"], registry=registry)
    counter.labels(bob="cat")
    gauge = Gauge("gauge_multiple", "desc", required_labels=["bob"], registry=registry)
    summary = Summary("summary_multiple", "desc", required_labels=["bob"], registry=registry)
    histogram = Histogram("histogram_multiple", "desc", required_labels=["bob"], registry=registry)
    if extra_label:
        counter.labels(bob="created_only_on_one").inc(3.0)
        gauge.labels(bob="observable_only_on_one").inc(2.7)
        summary.labels(bob="observable_only_on_one").observe(2.7)
        histogram.labels(bob="observable_only_on_one").observe(2.7)
    time.sleep(0.1)
    return generate_metrics(registry)


def test_multiple_return_all_metrics_entries():
    """
    Test that if a metric labeled child is created on a process, it will be retrieved even if the
    instance doesn't exist on a different process.
    """
    with ProcessPoolExecutor() as executor:
        first_result = executor.submit(_run_multiprocess, extra_label=True)
        first_result = first_result.result()
        second_result = executor.submit(_run_multiprocess, extra_label=False)
        second_result = second_result.result()

        assert first_result == second_result


class TestGenerateSamples:
    def test_counter(self):
        registry = CollectorRegistry()
        counter = Counter("counter", "desc", registry=registry)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP counter desc\n"
            "# TYPE counter counter\n"
            'counter 0.0\n'
        )

    def test_counter_labeled(self):
        registry = CollectorRegistry()
        counter = Counter("counter", "desc", required_labels=["bob"], registry=registry)
        counter.labels(bob="cat").inc(2.7)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP counter desc\n"
            "# TYPE counter counter\n"
            'counter{bob="cat"} 2.7\n'
        )

    def test_gauge(self):
        registry = CollectorRegistry()
        gauge = Gauge("gauge", "desc", registry=registry)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP gauge desc\n"
            "# TYPE gauge gauge\n"
            'gauge 0.0\n'
        )

    def test_gauge_labeled(self):
        registry = CollectorRegistry()
        gauge = Gauge("gauge", "desc", required_labels=["bob"], registry=registry)
        gauge.labels(bob="cat").inc(2.7)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP gauge desc\n"
            "# TYPE gauge gauge\n"
            'gauge{bob="cat"} 2.7\n'
        )

    def test_metric_labeled_multiple(self):
        registry = CollectorRegistry()
        counter_labeled = Counter(
            "counter_labeled", "desc", required_labels=["bob"], registry=registry
        )
        counter_labeled.labels(bob="cat").inc(2.7)
        gauge = Gauge("gauge", "desc", required_labels=["bob"], registry=registry)
        gauge.labels(bob="gage").inc(3.0)
        gauge.labels(bob="blob").inc(3.2)
        counter = Counter("counter", "desc", registry=registry)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP counter_labeled desc\n"
            "# TYPE counter_labeled counter\n"
            'counter_labeled{bob="cat"} 2.7\n'
            "# HELP gauge desc\n"
            "# TYPE gauge gauge\n"
            'gauge{bob="blob"} 3.2\n'
            'gauge{bob="gage"} 3.0\n'
            "# HELP counter desc\n"
            "# TYPE counter counter\n"
            'counter 0.0\n'
        )

    def test_summary(self):
        registry = CollectorRegistry()
        summary = Summary("summary", "desc", registry=registry)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP summary desc\n"
            "# TYPE summary summary\n"
            'summary_count 0.0\n'
            'summary_sum 0.0\n'
        )

    def test_summary_labeled(self):
        registry = CollectorRegistry()
        summary = Summary(
            "summary",
            "desc",
            registry=registry,
            required_labels=["bob"],
            default_labels={"bob": "cat"},
        )
        summary.observe(7)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP summary desc\n"
            "# TYPE summary summary\n"
            'summary_count{bob="cat"} 1.0\n'
            'summary_sum{bob="cat"} 7.0\n'
        )

    def test_histogram(self):
        registry = CollectorRegistry()
        histogram = Histogram("histogram", "desc", buckets=[1, 2, 3], registry=registry)
        histogram.observe(2.7)
        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP histogram desc\n"
            "# TYPE histogram histogram\n"
            'histogram_bucket{le="1"} 0.0\n'
            'histogram_bucket{le="2"} 0.0\n'
            'histogram_bucket{le="3"} 1.0\n'
            'histogram_bucket{le="+Inf"} 1.0\n'
            'histogram_count 1.0\n'
            'histogram_sum 2.7\n'
        )

    def test_histogram_labeled(self):
        registry = CollectorRegistry()
        histogram = Histogram(
            "histogram", "desc", buckets=[1, 2, 3], required_labels=["bob"], registry=registry
        )
        histogram.labels(bob="cat").observe(2.7)

        time.sleep(0.1)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP histogram desc\n"
            "# TYPE histogram histogram\n"
            'histogram_bucket{bob="cat",le="1"} 0.0\n'
            'histogram_bucket{bob="cat",le="2"} 0.0\n'
            'histogram_bucket{bob="cat",le="3"} 1.0\n'
            'histogram_bucket{bob="cat",le="+Inf"} 1.0\n'
            'histogram_count{bob="cat"} 1.0\n'
            'histogram_sum{bob="cat"} 2.7\n'
        )

    def test_labeled_histogram_is_ordered(self):
        registry = CollectorRegistry()
        histogram = Histogram(
            "histogram", "desc", buckets=[1, 2, 3], required_labels=["bob"], registry=registry
        )
        histogram.labels(bob="cat")
        histogram.labels(bob="bobby")
        time.sleep(0.1)  # give time to write to redis
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP histogram desc\n"
            "# TYPE histogram histogram\n"
            'histogram_bucket{bob="bobby",le="1"} 0.0\n'
            'histogram_bucket{bob="bobby",le="2"} 0.0\n'
            'histogram_bucket{bob="bobby",le="3"} 0.0\n'
            'histogram_bucket{bob="bobby",le="+Inf"} 0.0\n'
            'histogram_count{bob="bobby"} 0.0\n'
            'histogram_sum{bob="bobby"} 0.0\n'
            'histogram_bucket{bob="cat",le="1"} 0.0\n'
            'histogram_bucket{bob="cat",le="2"} 0.0\n'
            'histogram_bucket{bob="cat",le="3"} 0.0\n'
            'histogram_bucket{bob="cat",le="+Inf"} 0.0\n'
            'histogram_count{bob="cat"} 0.0\n'
            'histogram_sum{bob="cat"} 0.0\n'
        )

    def test_labeled_summary_is_ordered(self):
        registry = CollectorRegistry()
        summary = Summary("summary", "desc", required_labels=["bob"], registry=registry)
        summary.labels(bob="cat")
        summary.labels(bob="bobby")
        time.sleep(0.1) # give time to write to redis
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP summary desc\n"
            "# TYPE summary summary\n"
            'summary_count{bob="bobby"} 0.0\n'
            'summary_sum{bob="bobby"} 0.0\n'
            'summary_count{bob="cat"} 0.0\n'
            'summary_sum{bob="cat"} 0.0\n'
        )

    def test_labeled_not_observable(self):
        registry = CollectorRegistry()
        Counter("counter", "desc", required_labels=["bob"], registry=registry)
        Gauge("gauge", "desc", required_labels=["bob"], registry=registry)
        Summary("summary", "desc", required_labels=["bob"], registry=registry)
        Histogram("histogram", "desc", required_labels=["bob"], registry=registry)
        metrics_output = generate_metrics(registry)
        assert metrics_output == (
            "# HELP counter desc\n"
            "# TYPE counter counter\n"
            "# HELP gauge desc\n"
            "# TYPE gauge gauge\n"
            "# HELP summary desc\n"
            "# TYPE summary summary\n"
            "# HELP histogram desc\n"
            "# TYPE histogram histogram\n"
        )
