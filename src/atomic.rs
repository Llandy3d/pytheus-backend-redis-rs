use std::sync::atomic::{AtomicU64, Ordering};

pub struct AtomicF64 {
    inner_atomic: AtomicU64,
}

/// Cannot be used in statics as `to_bits` is not yet stable for `const fn`
impl AtomicF64 {
    pub fn new(value: f64) -> Self {
        Self {
            inner_atomic: AtomicU64::new(value.to_bits()),
        }
    }

    pub fn load(&self, ordering: Ordering) -> f64 {
        let value = self.inner_atomic.load(ordering);
        f64::from_bits(value)
    }

    pub fn store(&self, value: f64, ordering: Ordering) {
        self.inner_atomic.store(value.to_bits(), ordering);
    }

    pub fn fetch_add(&self, value: f64, ordering: Ordering) -> f64 {
        // NOTE: only supporting Relaxed operations for now
        // if Acquire/Release is needed, either the store or load needs to be Relaxed
        if ordering != Ordering::Relaxed {
            panic!("fetch_add only supports Relaxed Ordering for now");
        }

        let mut old_value_bits = self.inner_atomic.load(ordering);
        loop {
            let old_value = f64::from_bits(old_value_bits);
            let new_value_bits = f64::to_bits(old_value + value);
            match self.inner_atomic.compare_exchange_weak(
                old_value_bits,
                new_value_bits,
                ordering,
                ordering,
            ) {
                Ok(_) => return old_value,
                Err(e) => old_value_bits = e,
            }
        }
    }

    pub fn fetch_sub(&self, value: f64, ordering: Ordering) -> f64 {
        self.fetch_add(-value, ordering)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn load() {
        let atom = AtomicF64::new(0.0);
        assert_eq!(atom.load(Ordering::Relaxed), 0.0);
    }

    #[test]
    fn store() {
        let atom = AtomicF64::new(0.0);
        atom.store(3.2, Ordering::Relaxed);
        assert_eq!(atom.load(Ordering::Relaxed), 3.2);
    }

    #[test]
    fn fetch_add() {
        let atom = AtomicF64::new(3.0);
        let old_value = atom.fetch_add(2.0, Ordering::Relaxed);
        assert_eq!(old_value, 3.0);
        assert_eq!(atom.load(Ordering::Relaxed), 5.0);
    }

    #[test]
    #[should_panic]
    fn fetch_add_panics_without_relaxed_ordering() {
        let atom = AtomicF64::new(3.0);
        atom.fetch_add(2.0, Ordering::Acquire);
    }

    #[test]
    fn fetch_sub() {
        let atom = AtomicF64::new(0.0);
        let old_value = atom.fetch_add(-3.0, Ordering::Relaxed);
        assert_eq!(old_value, 0.0);
        assert_eq!(atom.load(Ordering::Relaxed), -3.0);
    }

    #[test]
    #[should_panic]
    fn fetch_sub_panics_without_relaxed_ordering() {
        let atom = AtomicF64::new(0.0);
        atom.fetch_sub(-2.0, Ordering::Acquire);
    }
}
