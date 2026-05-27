use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct LeaseBook {
    current_readers: AtomicUsize,
    max_readers: usize,
    operator_connected: AtomicBool,
}

impl LeaseBook {
    pub(crate) fn new(max_readers: usize) -> Arc<Self> {
        Arc::new(Self {
            current_readers: AtomicUsize::new(0),
            max_readers,
            operator_connected: AtomicBool::new(false),
        })
    }

    pub(crate) fn operator_connected(&self) -> bool {
        self.operator_connected.load(Ordering::Acquire)
    }

    pub(crate) fn try_operator(self: &Arc<Self>) -> Option<OperatorLease> {
        self.operator_connected
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| OperatorLease {
                book: Arc::clone(self),
                active: true,
            })
    }

    pub(crate) fn try_read(self: &Arc<Self>) -> Option<ReadLease> {
        let mut current = self.current_readers.load(Ordering::Acquire);
        loop {
            if current >= self.max_readers {
                return None;
            }
            match self.current_readers.compare_exchange(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ReadLease {
                        book: Arc::clone(self),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn reader_count(&self) -> usize {
        self.current_readers.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) struct ReadLease {
    book: Arc<LeaseBook>,
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        self.book.current_readers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct OperatorLease {
    book: Arc<LeaseBook>,
    active: bool,
}

impl OperatorLease {
    fn release_operator_slot(&self) {
        self.book.operator_connected.store(false, Ordering::Release);
    }
}

impl Drop for OperatorLease {
    fn drop(&mut self) {
        if self.active {
            self.release_operator_slot();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LeaseBook;

    #[test]
    fn read_lease_tracks_count_until_drop() {
        let book = LeaseBook::new(1);
        let read = book.try_read().expect("read slot should be free");

        assert_eq!(book.reader_count(), 1);
        assert!(book.try_read().is_none());

        drop(read);
        assert_eq!(book.reader_count(), 0);
        assert!(book.try_read().is_some());
    }

    #[test]
    fn operator_lease_tracks_connected_state_until_drop() {
        let book = LeaseBook::new(1);
        let operator = book.try_operator().expect("operator slot should be free");

        assert!(book.operator_connected());
        assert!(book.try_operator().is_none());

        drop(operator);
        assert!(!book.operator_connected());
        assert!(book.try_operator().is_some());
    }
}
