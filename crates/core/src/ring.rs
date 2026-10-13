use std::collections::VecDeque;

/// Keeps the newest `capacity` items; pushing onto a full ring drops the oldest.
#[derive(Debug, Clone)]
pub struct Ring<T> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T> Ring<T> {
    /// Capacity 0 is treated as 1.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, item: T) -> Option<T> {
        let evicted = if self.buf.len() == self.cap {
            self.buf.pop_front()
        } else {
            None
        };
        self.buf.push_back(item);
        evicted
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.buf.len() == self.cap
    }

    /// Oldest to newest.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.buf.iter()
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut T> + ExactSizeIterator {
        self.buf.iter_mut()
    }

    pub fn latest(&self) -> Option<&T> {
        self.buf.back()
    }

    pub fn latest_mut(&mut self) -> Option<&mut T> {
        self.buf.back_mut()
    }

    pub fn oldest(&self) -> Option<&T> {
        self.buf.front()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Drops items from the oldest end while `pred` holds (e.g. expire a time window).
    pub fn drop_oldest_while(&mut self, mut pred: impl FnMut(&T) -> bool) {
        while let Some(front) = self.buf.front() {
            if !pred(front) {
                break;
            }
            self.buf.pop_front();
        }
    }
}

impl<T> Extend<T> for Ring<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for item in iter {
            self.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_capacity_and_reports_eviction() {
        let mut r = Ring::new(3);
        assert_eq!(r.capacity(), 3);
        assert!(r.is_empty());
        assert_eq!(r.push(1), None);
        assert_eq!(r.push(2), None);
        assert_eq!(r.push(3), None);
        assert!(r.is_full());
        assert_eq!(r.push(4), Some(1));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn wraps_around_and_keeps_order_oldest_to_newest() {
        let mut r = Ring::new(4);
        r.extend(1..=10);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![7, 8, 9, 10]);
        assert_eq!(
            r.iter().rev().copied().collect::<Vec<_>>(),
            vec![10, 9, 8, 7]
        );
        assert_eq!(r.oldest(), Some(&7));
        assert_eq!(r.latest(), Some(&10));
    }

    #[test]
    fn zero_capacity_becomes_one() {
        let mut r = Ring::new(0);
        r.push('a');
        assert_eq!(r.push('b'), Some('a'));
        assert_eq!(r.latest(), Some(&'b'));
    }

    #[test]
    fn drop_oldest_while_expires_a_prefix_only() {
        let mut r = Ring::new(10);
        r.extend([1, 2, 3, 10, 2]);
        r.drop_oldest_while(|v| *v < 5);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![10, 2]);
    }

    #[test]
    fn clear_and_mutable_access() {
        let mut r = Ring::new(2);
        r.extend([1, 2]);
        *r.latest_mut().unwrap() = 9;
        for v in r.iter_mut() {
            *v += 1;
        }
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![2, 10]);
        r.clear();
        assert!(r.is_empty());
    }
}
