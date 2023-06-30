import time
from pytest_redis import factories
from pytheus.backends import load_backend
from pytheus.metrics import Counter, Histogram
from pytheus.registry import CollectorRegistry
from pytheus_backend_rs import RedisBackend


redis_server = factories.redis_proc(port=9000)
redis_client = factories.redisdb('redis_server', decode=True)


def test_smoke(redis_client):
    load_backend(RedisBackend, {"host": "localhost", "port": 9000})
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
    assert backend.labels_hash == "cat"


def test_create_backend_labeled_with_default():
    counter = Counter("name", "desc", required_labels=["bob"], default_labels={"bob": "cat"})
    backend = RedisBackend({}, counter)

    assert backend.key_name == counter.name
    assert backend.histogram_bucket is None
    assert backend.labels_hash == "cat"


def test_create_backend_labeled_with_default_mixed():
    counter = Counter(
        "name", "desc", required_labels=["bob", "bobby"], default_labels={"bob": "cat"}
    )
    counter = counter.labels({"bobby": "fish"})
    backend = RedisBackend({}, counter)

    assert backend.key_name == counter.name
    assert backend.histogram_bucket is None
    assert backend.labels_hash == "cat-fish"


def test_create_backend_with_histogram_bucket():
    histogram_bucket = "+Inf"
    counter = Counter("name", "desc")
    backend = RedisBackend({}, counter, histogram_bucket=histogram_bucket)

    assert backend.key_name == f"{counter.name}:{histogram_bucket}"
    assert backend.histogram_bucket == histogram_bucket
    assert backend.labels_hash is None


def test_multiple_metrics_with_same_name_with_redis_overlap(redis_client):
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


def test_multiple_metrics_with_same_name_labeled_with_redis_do_overlap_on_shared_child(redis_client):
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
    samples = RedisBackend._generate_samples(registry)
    assert len(samples[counter._collector]) == 3
