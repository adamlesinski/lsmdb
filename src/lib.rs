use std::{
    cmp::Ordering,
    collections::{btree_map::Range, BTreeMap},
    iter::Peekable,
    ops::Bound,
    path::{Path, PathBuf},
};

#[derive(Debug, PartialEq)]
pub struct DBError;

pub type Key = String;
pub type Value = Vec<u8>;

#[derive(Clone)]
enum Entry {
    Present(Value),
    Deleted,
}

impl Entry {
    /// Returns the entry's size on disk.
    pub fn len(&self) -> usize {
        match self {
            Entry::Present(value) => value.len(),
            Entry::Deleted => 1,
        }
    }
}

const MEMTABLE_MAX_SIZE_BYTES: usize = 1024 * 1024 * 1; // 1 MB size threshold

pub struct DB {
    root_path: PathBuf,

    memtable: BTreeMap<Key, Entry>,
    memtable_frozen: Option<BTreeMap<Key, Entry>>,

    // number of bytes that memtable has taken up so far
    // accounts for key and value size.
    memtable_size: usize,
}

impl DB {
    // `path` is a directory
    pub fn open(path: &Path) -> Result<DB, DBError> {
        Ok(DB {
            root_path: path.into(),
            memtable: BTreeMap::new(),
            memtable_frozen: None,
            memtable_size: 0,
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<Value>, DBError> {
        let mut result: Option<Entry> = self._get_from_memtable(key, &self.memtable)?;
        if result.is_none() {
            if let Some(snapshot) = self.memtable_frozen.as_ref() {
                result = self._get_from_memtable(key, snapshot)?;
            }
        }

        Ok(match result {
            Some(Entry::Present(data)) => Some(data.clone()),
            Some(Entry::Deleted) | None => None,
        })
    }

    fn _get_from_memtable(
        &self,
        key: &str,
        memtable: &BTreeMap<Key, Entry>,
    ) -> Result<Option<Entry>, DBError> {
        Ok(memtable.get(key).cloned())
    }

    fn _put_entry(&mut self, key: Key, entry: Entry) -> Result<(), DBError> {
        let key_len = key.as_bytes().len();
        let value_len = entry.len();
        self.memtable_size += value_len;
        match self.memtable.insert(key, entry) {
            Some(old_value) => {
                self.memtable_size -= old_value.len();
            }
            None => {
                self.memtable_size += key_len;
            }
        }
        if self.memtable_size >= MEMTABLE_MAX_SIZE_BYTES {
            self._swap_and_compact();
        }
        Ok(())
    }

    pub fn put(&mut self, key: impl Into<Key>, value: impl Into<Value>) -> Result<(), DBError> {
        self._put_entry(key.into(), Entry::Present(value.into()))
    }

    pub fn delete(&mut self, key: impl Into<Key>) -> Result<(), DBError> {
        self._put_entry(key.into(), Entry::Deleted)
    }

    pub fn seek(&self, prefix: &str) -> Result<DBIterator, DBError> {
        Ok(DBIterator {
            iter_memtable_mut: self
                .memtable
                .range((Bound::Included(prefix.to_string()), Bound::Unbounded))
                .peekable(),
            iter_memtable_immut: self.memtable_frozen.as_ref().map(|memtable| {
                memtable
                    .range((Bound::Included(prefix.to_string()), Bound::Unbounded))
                    .peekable()
            }),
            prefix: prefix.to_string(),
        })
    }

    fn _swap_and_compact(&mut self) {
        assert!(self.memtable_frozen.is_none());
        self.memtable_frozen = Some(std::mem::take(&mut self.memtable));
    }
}

pub struct DBIterator<'a> {
    iter_memtable_mut: Peekable<Range<'a, Key, Entry>>,
    iter_memtable_immut: Option<Peekable<Range<'a, Key, Entry>>>,
    prefix: Key,
}

impl<'a> Iterator for DBIterator<'a> {
    type Item = (Key, Value);

    fn next(&mut self) -> Option<Self::Item> {
        // We may need to skip deleted items, so iterate the inner iterator in a loop.
        loop {
            // Peek at both iterators and see which comes next.
            let (key, value) = match (
                self.iter_memtable_mut.peek(),
                self.iter_memtable_immut
                    .as_mut()
                    .map(|i| i.peek())
                    .flatten(),
            ) {
                // Both iterators have a value, check which takes precedence.
                (Some((key_mut, _value_mut)), Some((key_immut, _value_immut))) => {
                    match key_mut.cmp(key_immut) {
                        Ordering::Equal => {
                            // The left (mutable) key takes precedence over the right (immutable).
                            // Skip the stale value in the immutable iterator.
                            let _ = self.iter_memtable_immut.as_mut().unwrap().next();
                            self.iter_memtable_mut.next().unwrap()
                        }
                        Ordering::Less => {
                            // Consume the left (mutable) value first
                            self.iter_memtable_mut.next().unwrap()
                        }
                        Ordering::Greater => {
                            // Consume the right (immutable) value first
                            self.iter_memtable_immut.as_mut().unwrap().next().unwrap()
                        }
                    }
                }
                // Only the left iterator (mutable) has a value, take it as-is.
                (Some((_key, _value)), None) => self.iter_memtable_mut.next().unwrap(),
                // Only the right iterator (immutable) has a value, take it as-is.
                (None, Some((_key, _value))) => {
                    self.iter_memtable_immut.as_mut().unwrap().next().unwrap()
                }
                // Both iterators are exhausted, terminate.
                (None, None) => return None,
            };
            // The underlying iterator iterates over a range that is unbounded, so we need to
            // check when the keys stop matching the desired prefix.
            if !key.starts_with(&self.prefix) {
                // Terminate iteration. This is enough to satisfy the iterator protocol; we don't
                // need to mark any internal state that iteration is ended.
                return None;
            }
            match value {
                Entry::Present(data) => return Some((key.clone(), data.clone())),
                Entry::Deleted => {
                    // The key was deleted, so skip it and fetch the next value.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let mut db = DB::open(Path::new("/tmp/hello")).expect("failed to open");

        db.put("1", "hello").expect("cant put 1");
        db.put("2", "world").expect("cant put 2");
        assert_eq!(db.get("1"), Ok(Some("hello".as_bytes().to_vec())));
        assert_eq!(db.get("2"), Ok(Some("world".as_bytes().to_vec())));
        assert_eq!(db.get("3"), Ok(None));
    }

    #[test]
    fn basic_delete() {
        let mut db = DB::open(Path::new("/tmp/hello")).expect("failed to open");

        db.put("1", "hello").expect("cant put 1");
        db.put("2", "world").expect("cant put 2");

        assert_eq!(db.get("2"), Ok(Some(b"world".to_vec())));
        db.delete("2").expect("couldnt delete 2");
        assert_eq!(db.get("2").expect("cant put 2"), None);
    }

    #[test]
    fn basic_seek() {
        let mut db = DB::open(Path::new("/tmp/hello")).expect("failed to open");

        db.put("/user/name/adam", "adam")
            .expect("cant put /user/adam");
        db.put("/user/name/vardhan", "vardhan")
            .expect("cant put /user/vardhan");
        db.put("/abc", "abc").expect("cant put /abc");
        db.put("/xyz", "xyz").expect("cant put /xyz");
        assert_eq!(db.get("/user/name/vardhan"), Ok(Some(b"vardhan".to_vec())));

        assert_eq!(
            db.seek("/user/")
                .expect("couldnt seek /user")
                .collect::<Vec<(Key, Value)>>(),
            vec![
                ("/user/name/adam".to_string(), b"adam".to_vec()),
                ("/user/name/vardhan".to_string(), b"vardhan".to_vec())
            ]
        );

        assert_eq!(
            db.seek("/user/vardhan_")
                .expect("couldn't seen /user/vardhan_")
                .collect::<Vec<(Key, Value)>>(),
            vec![]
        );

        assert_eq!(
            db.seek("/items/")
                .expect("couldnt seek /items")
                .collect::<Vec<(Key, Value)>>(),
            vec![]
        );
    }

    #[test]
    fn seek_with_frozen_memtable() {
        let mut db = DB::open(Path::new("/tmp/hello")).expect("failed to open");

        db.put("/user/name/adam", "adam")
            .expect("cant put /user/adam");
        db.put("/user/name/vardhan", "vardhan")
            .expect("cant put /user/vardhan");
        db.put("/user/name/catherine", "catherine")
            .expect("cant put /user/catherine");
        db.put("/abc", "abc").expect("cant put /abc");
        db.put("/xyz", "xyz").expect("cant put /xyz");

        db._swap_and_compact();

        assert_eq!(
            db.seek("/user/")
                .expect("couldnt seek /user")
                .collect::<Vec<(Key, Value)>>(),
            vec![
                ("/user/name/adam".to_string(), b"adam".to_vec()),
                ("/user/name/catherine".to_string(), b"catherine".to_vec()),
                ("/user/name/vardhan".to_string(), b"vardhan".to_vec())
            ]
        );

        db.delete("/user/name/catherine")
            .expect("couldnt delete /user/catherine");

        db.put("/user/name/adam", "vardhan")
            .expect("cant put /user/name/adam");

        assert_eq!(db.get("/user/name/vardhan"), Ok(Some(b"vardhan".to_vec())));

        assert_eq!(
            db.seek("/user/")
                .expect("couldnt seek /user")
                .collect::<Vec<(Key, Value)>>(),
            vec![
                ("/user/name/adam".to_string(), b"vardhan".to_vec()),
                ("/user/name/vardhan".to_string(), b"vardhan".to_vec())
            ]
        );

        assert_eq!(
            db.seek("/user/vardhan_")
                .expect("couldn't seen /user/vardhan_")
                .collect::<Vec<(Key, Value)>>(),
            vec![]
        );

        assert_eq!(
            db.seek("/items/")
                .expect("couldnt seek /items")
                .collect::<Vec<(Key, Value)>>(),
            vec![]
        );
    }
}
