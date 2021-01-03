#![allow(unsafe_code)]

// TODO we can skip the first offset because it's always 0

use std::{
    alloc::{alloc_zeroed, dealloc, Layout},
    cmp::Ordering::{Equal, Greater, Less},
    convert::{TryFrom, TryInto},
    fmt,
    mem::{align_of, size_of, ManuallyDrop},
    num::NonZeroU64,
    ops::{Bound, Deref, DerefMut},
};

use crate::{varint, IVec, Link};

const ALIGNMENT: usize = align_of::<Header>();

// allocates space for a header struct at the beginning.
pub(crate) fn aligned_boxed_slice(items_size: usize) -> Box<[u8]> {
    let size = items_size + size_of::<Header>();
    let layout = Layout::from_size_align(size, ALIGNMENT).unwrap();

    unsafe {
        let ptr = alloc_zeroed(layout);
        let fat_ptr = fatten(ptr, size);
        let ret = Box::from_raw(fat_ptr);
        assert_eq!(ret.len(), size);
        ret
    }
}

/// <https://users.rust-lang.org/t/construct-fat-pointer-to-struct/29198/9>
#[allow(trivial_casts)]
fn fatten(data: *const u8, len: usize) -> *mut [u8] {
    // Requirements of slice::from_raw_parts.
    assert!(!data.is_null());
    assert!(isize::try_from(len).is_ok());

    let slice = unsafe { core::slice::from_raw_parts(data as *const (), len) };
    slice as *const [()] as *mut _
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    // NB always lay out fields from largest to smallest
    // to properly pack the struct
    pub next: Option<NonZeroU64>,
    // could probably be Option<u16> w/ child index
    // rather than the pid
    pub merging_child: Option<NonZeroU64>,
    // could be replaced by a varint, w/ data buf offset stored instead
    lo_len: u64,
    // could be replaced by a varint, w/ data buf offset stored instead
    hi_len: u64,
    // can probably be NonZeroU16
    fixed_key_length: Option<NonZeroU64>,
    // can probably be NonZeroU16
    fixed_value_length: Option<NonZeroU64>,
    pub children: u16,
    pub prefix_len: u8,
    probation_ops_remaining: u8,
    // this can be 3 bits. 111 = 7, but we
    // will never need 7 bytes for storing offsets.
    // address spaces cap out at 2 ** 48 (256 ** 6)
    // so as long as we can represent the numbers 1-6,
    // we can reach the full linux address space currently
    // supported as of 2021.
    offset_bytes: u8,
    // can be 2 bits
    pub rewrite_generations: u8,
    // this can really be 2 bits, representing
    // 00: all updates have been at the end
    // 01: mixed updates
    // 10: all updates have been at the beginning
    activity_sketch: u8,
    // can be 1 bit
    pub merging: bool,
    // can be 1 bit
    pub is_index: bool,
}

/// An immutable sorted string table
#[must_use]
#[derive(Clone)]
#[cfg_attr(feature = "testing", derive(PartialEq))]
pub struct Node(pub ManuallyDrop<Box<[u8]>>);

impl Drop for Node {
    fn drop(&mut self) {
        let box_ptr = self.0.as_mut_ptr();
        let layout = Layout::from_size_align(self.0.len(), ALIGNMENT).unwrap();
        unsafe {
            dealloc(box_ptr, layout);
        }
    }
}

impl Deref for Node {
    type Target = Header;

    fn deref(&self) -> &Header {
        self.header()
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ds = f.debug_struct("Node");

        ds.field("header", self.header())
            .field("lo", &self.lo())
            .field("hi", &self.hi());

        if self.is_index {
            ds.field(
                "items",
                &self
                    .iter_keys()
                    .zip(self.iter_index_pids())
                    .collect::<Vec<_>>(),
            )
            .finish()
        } else {
            ds.field("items", &self.iter().collect::<Vec<_>>()).finish()
        }
    }
}

impl DerefMut for Node {
    fn deref_mut(&mut self) -> &mut Header {
        self.header_mut()
    }
}

impl Node {
    pub unsafe fn from_raw(buf: &[u8]) -> Node {
        let mut boxed_slice =
            aligned_boxed_slice(buf.len() - size_of::<Header>());
        boxed_slice.copy_from_slice(buf);
        Node(ManuallyDrop::new(boxed_slice))
    }

    pub(crate) fn new(
        lo: &[u8],
        hi: Option<&[u8]>,
        prefix_len: u8,
        is_index: bool,
        next: Option<NonZeroU64>,
        items: &[(&[u8], &[u8])],
    ) -> Node {
        assert!(items.len() <= std::u16::MAX as usize);

        // determine if we need to use varints and offset
        // indirection tables, or if everything is equal
        // size we can skip this.
        let mut key_lengths = Vec::with_capacity(items.len());
        let mut value_lengths = Vec::with_capacity(items.len());

        let mut initial_keys_equal_length = true;
        let mut initial_values_equal_length = true;
        for (k, v) in items {
            key_lengths.push(k.len() as u64);
            if let Some(first_sz) = key_lengths.first() {
                initial_keys_equal_length &= *first_sz == k.len() as u64;
            }
            value_lengths.push(v.len() as u64);
            if let Some(first_sz) = value_lengths.first() {
                if is_index {
                    assert_eq!(*first_sz, size_of::<u64>() as u64);
                }
                initial_values_equal_length &= *first_sz == v.len() as u64;
            }
        }

        let (fixed_key_length, keys_equal_length) = if initial_keys_equal_length
        {
            if let Some(key_length) = key_lengths.first() {
                if *key_length > 0 {
                    (Some(NonZeroU64::new(*key_length).unwrap()), true)
                } else {
                    (None, false)
                }
            } else {
                (None, false)
            }
        } else {
            (None, false)
        };

        let (fixed_value_length, values_equal_length) =
            if initial_values_equal_length {
                if let Some(value_length) = value_lengths.first() {
                    if *value_length > 0 {
                        (Some(NonZeroU64::new(*value_length).unwrap()), true)
                    } else {
                        (None, false)
                    }
                } else {
                    (None, false)
                }
            } else {
                (None, false)
            };

        let key_storage_size = if let Some(key_length) = fixed_key_length {
            key_length.get() * (items.len() as u64)
        } else {
            let mut sum = 0;
            for key_length in &key_lengths {
                sum += key_length;
                sum += varint::size(*key_length);
            }
            sum
        };

        let value_storage_size = if let Some(value_length) = fixed_value_length
        {
            value_length.get() * (items.len() as u64)
        } else {
            let mut sum = 0;
            for value_length in &value_lengths {
                sum += value_length;
                sum += varint::size(*value_length);
            }
            sum
        };

        let (offsets_storage_size, offset_bytes) = if keys_equal_length
            && values_equal_length
        {
            (0, 0)
        } else {
            let max_offset_storage_size = (6 * items.len()) as u64;
            let max_total_item_storage_size =
                key_storage_size + value_storage_size + max_offset_storage_size;

            let bytes_per_offset: u8 = match max_total_item_storage_size {
                i if i < 256 => 1,
                i if i < (1 << 16) => 2,
                i if i < (1 << 24) => 3,
                i if i < (1 << 32) => 4,
                i if i < (1 << 40) => 5,
                i if i < (1 << 48) => 6,
                _ => unreachable!(),
            };

            (
                u64::try_from(bytes_per_offset).unwrap() * items.len() as u64,
                bytes_per_offset,
            )
        };

        let total_item_storage_size = hi.map(|hi| hi.len() as u64).unwrap_or(0)
            + lo.len() as u64
            + key_storage_size
            + value_storage_size
            + offsets_storage_size;

        let boxed_slice = aligned_boxed_slice(
            usize::try_from(total_item_storage_size).unwrap(),
        );

        let mut ret = Node(ManuallyDrop::new(boxed_slice));

        *ret.header_mut() = Header {
            rewrite_generations: 0,
            activity_sketch: 0,
            probation_ops_remaining: 0,
            merging_child: None,
            merging: false,
            lo_len: lo.len() as u64,
            hi_len: hi.map(|hi| hi.len() as u64).unwrap_or(0),
            fixed_key_length,
            fixed_value_length,
            offset_bytes,
            children: u16::try_from(items.len()).unwrap(),
            prefix_len,
            next,
            is_index,
        };

        ret.lo_mut().copy_from_slice(lo);

        if let Some(ref mut hi_buf) = ret.hi_mut() {
            hi_buf.copy_from_slice(hi.unwrap());
        }

        // we use either 0 or 1 offset tables.
        // - if keys and values are all equal lengths, no offset table is
        //   required
        // - if keys are equal length but values are not, we put an offset table
        //   at the beginning of the data buffer, then put each of the keys
        //   packed together, then varint-prefixed values which are addressed by
        //   the offset table
        // - if keys and values are both different lengths, we put an offset
        //   table at the beginning of the data buffer, then varint-prefixed
        //   keys followed inline with varint-prefixed values.
        //
        // So, there are 4 possible layouts:
        // 1. [fixed size keys] [fixed size values]
        //  - signified by fixed_key_length and fixed_value_length being Some
        // 2. [offsets] [fixed size keys] [variable values]
        //  - fixed_key_length: Some, fixed_value_length: None
        // 3. [offsets] [variable keys] [fixed-length values]
        //  - fixed_key_length: None, fixed_value_length: Some
        // 4. [offsets] [variable keys followed by variable values]
        //  - fixed_key_length: None, fixed_value_length: None
        let mut offset = 0_u64;
        for (idx, (k, v)) in items.iter().enumerate() {
            if !keys_equal_length || !values_equal_length {
                ret.set_offset(idx, usize::try_from(offset).unwrap());
            }
            if !keys_equal_length {
                offset += varint::size(k.len() as u64) + k.len() as u64;
            }
            if !values_equal_length {
                offset += varint::size(v.len() as u64) + v.len() as u64;
            }

            let mut key_buf = ret.key_buf_for_offset_mut(idx);
            if !keys_equal_length {
                let varint_bytes =
                    varint::serialize_into(k.len() as u64, key_buf);
                key_buf = &mut key_buf[varint_bytes..];
            }
            key_buf[..k.len()].copy_from_slice(k);

            let mut value_buf = ret.value_buf_for_offset_mut(idx);
            if !values_equal_length {
                let varint_bytes =
                    varint::serialize_into(v.len() as u64, value_buf);
                value_buf = &mut value_buf[varint_bytes..];
            }
            value_buf[..v.len()].copy_from_slice(v);
        }

        testing_assert!(ret.is_sorted());

        ret
    }

    pub(crate) fn new_root(child_pid: u64) -> Node {
        Node::new(
            &[],
            None,
            0,
            true,
            None,
            &[(prefix::empty(), &child_pid.to_le_bytes())],
        )
    }

    pub(crate) fn new_hoisted_root(left: u64, at: &[u8], right: u64) -> Node {
        Node::new(
            &[],
            None,
            0,
            true,
            None,
            &[
                (prefix::empty(), &left.to_le_bytes()),
                (at, &right.to_le_bytes()),
            ],
        )
    }

    // returns the OPEN ENDED buffer where a key may be placed
    fn key_buf_for_offset_mut(&mut self, index: usize) -> &mut [u8] {
        let offset_sz = self.children as usize * self.offset_bytes as usize;
        match (self.fixed_key_length, self.fixed_value_length) {
            (Some(k_sz), Some(_)) | (Some(k_sz), None) => {
                let keys_buf = &mut self.data_buf_mut()[offset_sz..];
                &mut keys_buf[index * usize::try_from(k_sz.get()).unwrap()..]
            }
            (None, Some(_)) | (None, None) => {
                // find offset for key or combined kv offset
                let offset = self.offset(index);
                let keys_buf = &mut self.data_buf_mut()[offset_sz..];
                &mut keys_buf[offset..]
            }
        }
    }

    // returns the OPEN ENDED buffer where a value may be placed
    //
    // NB: it's important that this is only ever called after setting
    // the key and its varint length prefix, as this needs to be parsed
    // for case 4.
    fn value_buf_for_offset_mut(&mut self, index: usize) -> &mut [u8] {
        match (self.fixed_key_length, self.fixed_value_length) {
            (Some(_), Some(v_sz)) | (None, Some(v_sz)) => {
                let values_buf = self.values_buf_mut();
                &mut values_buf[index * usize::try_from(v_sz.get()).unwrap()..]
            }
            (Some(_), None) => {
                // find combined kv offset
                let offset = self.offset(index);
                let values_buf = self.values_buf_mut();
                &mut values_buf[offset..]
            }
            (None, None) => {
                // find combined kv offset, skip key bytes
                let offset = self.offset(index);
                let values_buf = self.values_buf_mut();
                let slot_buf = &mut values_buf[offset..];
                let (val_len, varint_sz) =
                    varint::deserialize(slot_buf).unwrap();
                &mut slot_buf[usize::try_from(val_len).unwrap() + varint_sz..]
            }
        }
    }

    // returns the OPEN ENDED buffer where a value may be read
    //
    // NB: it's important that this is only ever called after setting
    // the key and its varint length prefix, as this needs to be parsed
    // for case 4.
    fn value_buf_for_offset(&self, index: usize) -> &[u8] {
        match (self.fixed_key_length, self.fixed_value_length) {
            (Some(_), Some(v_sz)) | (None, Some(v_sz)) => {
                let values_buf = self.values_buf();
                &values_buf[index * usize::try_from(v_sz.get()).unwrap()..]
            }
            (Some(_), None) => {
                // find combined kv offset
                let offset = self.offset(index);
                let values_buf = self.values_buf();
                &values_buf[offset..]
            }
            (None, None) => {
                // find combined kv offset, skip key bytes
                let offset = self.offset(index);
                let values_buf = self.values_buf();
                let slot_buf = &values_buf[offset..];
                let (val_len, varint_sz) =
                    varint::deserialize(slot_buf).unwrap();
                &slot_buf[usize::try_from(val_len).unwrap() + varint_sz..]
            }
        }
    }

    fn offset(&self, index: usize) -> usize {
        assert!(index < self.children as usize);
        assert!(self.offset_bytes > 0);
        let offsets_buf_start = usize::try_from(self.lo_len).unwrap()
            + usize::try_from(self.hi_len).unwrap()
            + size_of::<Header>();

        let start = offsets_buf_start + (index * self.offset_bytes as usize);
        let mask = std::usize::MAX
            >> (8
                * (u32::try_from(size_of::<usize>()).unwrap()
                    - u32::from(self.offset_bytes)));

        // we use unsafe code here because it cuts around 5% of CPU cycles
        // on a simple insertion workload compared to using the more
        // idiomatic approach of copying the correct number of bytes into
        // a buffer initialized with zeroes. the seemingly "less" unsafe
        // approach of using ptr::copy_nonoverlapping did not improve matters.
        // using a match statement on offest_bytes and performing simpler
        // casting for one or two bytes slowed things down due to increasing
        // code size. this approach is branch-free and cut CPU usage of this
        // function from 7-11% down to 2-3% in a monotonic insertion workload.
        #[allow(unsafe_code)]
        unsafe {
            let ptr: *const u8 = self.0.as_ptr().add(start);
            let cast_ptr = ptr as *const usize;
            cast_ptr.read_unaligned() & mask
        }
    }

    fn set_offset(&mut self, index: usize, offset: usize) {
        let offset_bytes = self.offset_bytes as usize;
        let buf = {
            let start = index * self.offset_bytes as usize;
            let end = start + offset_bytes;
            &mut self.data_buf_mut()[start..end]
        };
        let bytes = &offset.to_le_bytes()[..offset_bytes];
        buf.copy_from_slice(bytes);
    }

    fn values_buf_mut(&mut self) -> &mut [u8] {
        let offset_sz = self.children as usize * self.offset_bytes as usize;
        match (self.fixed_key_length, self.fixed_value_length) {
            (Some(fixed_key_length), Some(_))
            | (Some(fixed_key_length), None) => {
                let start = offset_sz
                    + usize::try_from(fixed_key_length.get()).unwrap()
                        * self.children as usize;
                &mut self.data_buf_mut()[start..]
            }
            (None, Some(fixed_value_length)) => {
                let total_value_size =
                    usize::try_from(fixed_value_length.get()).unwrap()
                        * self.children as usize;
                let data_buf = self.data_buf_mut();
                let start = data_buf.len() - total_value_size;
                &mut data_buf[start..]
            }
            (None, None) => &mut self.data_buf_mut()[offset_sz..],
        }
    }

    fn values_buf(&self) -> &[u8] {
        let offset_sz = self.children as usize * self.offset_bytes as usize;
        match (self.fixed_key_length, self.fixed_value_length) {
            (Some(fixed_key_length), Some(_))
            | (Some(fixed_key_length), None) => {
                let start = offset_sz
                    + usize::try_from(fixed_key_length.get()).unwrap()
                        * self.children as usize;
                &self.data_buf()[start..]
            }
            (None, Some(fixed_value_length)) => {
                let total_value_size =
                    usize::try_from(fixed_value_length.get()).unwrap()
                        * self.children as usize;
                let data_buf = self.data_buf();
                let start = data_buf.len() - total_value_size;
                &data_buf[start..]
            }
            (None, None) => &self.data_buf()[offset_sz..],
        }
    }

    #[inline]
    fn data_buf(&self) -> &[u8] {
        let start = usize::try_from(self.lo_len).unwrap()
            + usize::try_from(self.hi_len).unwrap()
            + size_of::<Header>();
        &self.0[start..]
    }

    fn data_buf_mut(&mut self) -> &mut [u8] {
        let start = usize::try_from(self.lo_len).unwrap()
            + usize::try_from(self.hi_len).unwrap()
            + size_of::<Header>();
        &mut self.0[start..]
    }

    pub(crate) fn apply(&self, link: &Link) -> Node {
        use self::Link::*;

        assert!(
            !self.merging,
            "somehow a link was applied to a node after it was merged"
        );

        match *link {
            Set(ref k, ref v) => self.insert(k, v),
            Replace(index, ref v) => self.replace(index, v),
            Del(index) => self.remove_index(index),
            ParentMergeIntention(pid) => {
                assert!(
                    self.can_merge_child(pid),
                    "trying to merge {:?} into node {:?} which \
                     is not a valid merge target",
                    link,
                    self
                );
                let mut clone = self.clone();
                clone.merging_child = Some(NonZeroU64::new(pid).unwrap());
                clone
            }
            ParentMergeConfirm => {
                assert!(self.merging_child.is_some());
                let merged_child = self
                    .merging_child
                    .expect(
                        "we should have a specific \
                     child that was merged if this \
                     link appears here",
                    )
                    .get();
                let idx = self
                    .iter_index_pids()
                    .position(|pid| pid == merged_child)
                    .unwrap();
                let mut ret = self.remove_index(idx);
                ret.merging_child = None;
                ret
            }
            ChildMergeCap => {
                let mut ret = self.clone();
                ret.merging = true;
                ret
            }
        }
    }

    fn stitch(
        &self,
        index: usize,
        new_item: Option<(&[u8], &[u8])>,
        replace: bool,
    ) -> Node {
        log::trace!(
            "stitching item {:?} replace: {} index: {} \
            into node {:?}",
            new_item,
            replace,
            index,
            self,
        );

        // possible optimizations:
        // if replace && length remains the same
        //  simple copy self bytes
        // if fixed lengths and new item matches
        //  simple copy of predecessors, new item,
        //
        //
        // things that change
        //   fixed lengths
        //   offset

        let children = if new_item.is_none() {
            self.children - 1
        } else if replace {
            self.children
        } else {
            self.children + 1
        };

        // dbg!(children);

        let take_slow_path = if let Some((k, v)) = new_item {
            let new_max_sz = self.0.len()
                + varint::size(k.len() as u64) as usize
                + k.len()
                + varint::size(v.len() as u64) as usize
                + v.len()
                + 6;

            let new_offset_bytes = match new_max_sz {
                i if i < 256 => 1,
                i if i < (1 << 16) => 2,
                i if i < (1 << 24) => 3,
                i if i < (1 << 32) => 4,
                i if i < (1 << 40) => 5,
                i if i < (1 << 48) => 6,
                _ => unreachable!(),
            };

            let requires_offset_expansion =
                new_offset_bytes > self.offset_bytes;

            let violates_fixed_key_length =
                if let Some(fkl) = self.fixed_key_length {
                    fkl.get() != k.len() as u64
                } else {
                    false
                };

            let violates_fixed_value_length =
                if let Some(fvl) = self.fixed_value_length {
                    fvl.get() != v.len() as u64
                } else {
                    false
                };

            requires_offset_expansion
                || violates_fixed_key_length
                || violates_fixed_value_length
        } else {
            false
        };

        if take_slow_path {
            let items: Vec<_> = self
                .iter()
                .take(index)
                .chain(new_item)
                .chain(self.iter().skip(index + if replace { 1 } else { 0 }))
                .collect();

            let mut ret = Node::new(
                self.lo(),
                self.hi(),
                self.prefix_len,
                self.is_index,
                self.next,
                &items,
            );

            if ret.children > 1 {
                // if we have 1 existing child and our insert index is 1,
                // we want to set the max activity bit. if the index is 0
                // we want to set the min activity bit. as we get more
                // items, we generally want to set the bit that is
                // proportionally
                let activity_sketch_bit = if index == self.children as usize {
                    7
                } else {
                    (index * 8) / self.children as usize
                };
                assert!(activity_sketch_bit <= 7);
                let activity_byte = 1_u8 << activity_sketch_bit;
                ret.activity_sketch = activity_byte | self.activity_sketch;
            }

            testing_assert!(ret.is_sorted());

            return ret;
        }

        let existing_item_size = if replace {
            let k = self.index_key(index);
            let v = self.index_value(index);

            self.offset_bytes as usize
                + k.len()
                + v.len()
                + if self.fixed_key_length.is_some() {
                    0
                } else {
                    varint::size(k.len() as u64) as usize
                }
                + if self.fixed_value_length.is_some() {
                    0
                } else {
                    varint::size(v.len() as u64) as usize
                }
        } else {
            0
        };

        let new_item_size = if let Some((k, v)) = new_item {
            self.offset_bytes as usize
                + k.len()
                + v.len()
                + if self.fixed_key_length.is_some() {
                    0
                } else {
                    varint::size(k.len() as u64) as usize
                }
                + if self.fixed_value_length.is_some() {
                    0
                } else {
                    varint::size(v.len() as u64) as usize
                }
        } else {
            0
        };

        let diff: isize = new_item_size as isize - existing_item_size as isize;

        let allocation_size = (self.0.len() as isize + diff) as usize;

        let mut ret = Node(ManuallyDrop::new(aligned_boxed_slice(
            allocation_size - size_of::<Header>(),
        )));

        *ret.header_mut() = Header {
            children,
            probation_ops_remaining: self
                .probation_ops_remaining
                .saturating_sub(1),
            ..**self
        };

        // set lo and hi keys
        ret.lo_mut().copy_from_slice(self.lo());
        if let Some(ref mut hi_buf) = ret.hi_mut() {
            hi_buf.copy_from_slice(self.hi().unwrap());
        }

        if ret.offset_bytes > 0 {
            // set offsets, properly shifted after index
            let mut offset_shift: isize = if self.fixed_key_length.is_none() {
                let old_key_bytes = if replace {
                    let old_key = self.index_key(index);
                    old_key.len() + varint::size(old_key.len() as u64) as usize
                } else {
                    0
                };

                let new_key_bytes = if let Some((new_key, _)) = new_item {
                    new_key.len() + varint::size(new_key.len() as u64) as usize
                } else {
                    0
                };

                new_key_bytes as isize - old_key_bytes as isize
            } else {
                0
            };

            if self.fixed_value_length.is_none() {
                let old_value_bytes = if replace {
                    let old_value = self.index_value(index);
                    old_value.len()
                        + varint::size(old_value.len() as u64) as usize
                } else {
                    0
                };

                let new_value_bytes = if let Some((_, new_value)) = new_item {
                    new_value.len()
                        + varint::size(new_value.len() as u64) as usize
                } else {
                    0
                };

                let value_shift =
                    new_value_bytes as isize - old_value_bytes as isize;

                offset_shift += value_shift
            };

            // println!("offset_shift: {}", offset_shift);

            // just copy the offsets before the index
            let start = usize::try_from(ret.lo_len).unwrap()
                + usize::try_from(ret.hi_len).unwrap()
                + size_of::<Header>();
            let end = start + (index * ret.offset_bytes as usize);

            ret.0[start..end].copy_from_slice(&self.0[start..end]);

            let previous_offset =
                if index > 0 { ret.offset(index - 1) } else { 0 };

            let previous_item_size = if index > 0 {
                let mut previous_item_size = 0;
                if ret.fixed_key_length.is_none() {
                    let prev_key = self.index_key(index - 1);
                    previous_item_size += prev_key.len()
                        + varint::size(prev_key.len() as u64) as usize;
                }
                if ret.fixed_value_length.is_none() {
                    let prev_value = self.index_value(index - 1);
                    previous_item_size += prev_value.len()
                        + varint::size(prev_value.len() as u64) as usize;
                }
                previous_item_size
            } else {
                0
            };

            // set offset at index to previous index + previous size
            if children > 0 {
                ret.set_offset(index, previous_offset + previous_item_size);
            }

            /*
            for i in 0..self.children as usize {
                println!("self offset {}: {}", i, self.offset(i));
            }

            for i in 0..ret.children as usize {
                println!("pre-shift offset {}: {}", i, ret.offset(i));
            }
            */

            if ret.children > 0 {
                for i in (index + 1)..ret.children as usize {
                    // shift the old index down
                    //dbg!(i);
                    let old_offset = self.offset(if replace {
                        if new_item.is_some() {
                            i
                        } else {
                            i + 1
                        }
                    } else {
                        i - 1
                    });
                    let shifted_offset =
                        (old_offset as isize + offset_shift) as usize;
                    /*
                    println!(
                        "shifted offset at index {} from {} to {}",
                        i, old_offset, shifted_offset
                    );
                    */
                    ret.set_offset(i, shifted_offset);
                }
            }

            /*
            for i in 0..ret.children as usize {
                println!("post-shift offset {}: {}", i, ret.offset(i));
            }
            */
        }

        // write keys, possibly performing some copy optimizations
        if let Some(fixed_key_length) = self.fixed_key_length {
            let fixed_key_length = fixed_key_length.get() as usize;

            let self_offset_sz =
                self.children as usize * self.offset_bytes as usize;
            let self_keys_buf = &self.data_buf()[self_offset_sz..];

            let ret_offset_sz =
                ret.children as usize * ret.offset_bytes as usize;
            let ret_keys_buf = &mut ret.data_buf_mut()[ret_offset_sz..];

            let prelude = index * fixed_key_length;
            ret_keys_buf[..prelude].copy_from_slice(&self_keys_buf[..prelude]);

            let item_end =
                prelude + if new_item.is_some() { fixed_key_length } else { 0 };

            if let Some((k, _)) = new_item {
                ret_keys_buf[prelude..item_end].copy_from_slice(k);
            }

            let remaining_items =
                (children as usize) - (index + if replace { 0 } else { 1 });

            let ret_prologue_start = item_end;
            let ret_prologue_end =
                item_end + (remaining_items * fixed_key_length);

            let self_prologue_end = (self.children as usize) * fixed_key_length;
            let self_prologue_start =
                self_prologue_end - (remaining_items * fixed_key_length);

            ret_keys_buf[ret_prologue_start..ret_prologue_end].copy_from_slice(
                &self_keys_buf[self_prologue_start..self_prologue_end],
            );
        } else {
            for idx in 0..index {
                let k = self.index_key(idx);
                let mut key_buf = ret.key_buf_for_offset_mut(idx);
                //println!("1 writing key {:?} at {:?}", k, key_buf.as_ptr());
                let varint_bytes =
                    varint::serialize_into(k.len() as u64, key_buf);
                key_buf = &mut key_buf[varint_bytes..];
                key_buf[..k.len()].copy_from_slice(k);
            }

            if let Some((k, _)) = new_item {
                let mut key_buf = ret.key_buf_for_offset_mut(index);
                //println!("2 writing key {:?} at {:?}", k, key_buf.as_ptr());
                let varint_bytes =
                    varint::serialize_into(k.len() as u64, key_buf);
                key_buf = &mut key_buf[varint_bytes..];
                key_buf[..k.len()].copy_from_slice(k);
            }

            let start = index + if replace { 1 } else { 0 };

            // dbg!(start);
            for idx in start..self.children as usize {
                let self_idx = idx;
                let ret_idx = if replace {
                    if new_item.is_some() {
                        idx
                    } else {
                        idx - 1
                    }
                } else {
                    idx + 1
                };
                let k = self.index_key(self_idx);
                let mut key_buf = ret.key_buf_for_offset_mut(ret_idx);
                // println!("3 writing key {:?} at {:?}", k, key_buf.as_ptr());
                let varint_bytes =
                    varint::serialize_into(k.len() as u64, key_buf);
                key_buf = &mut key_buf[varint_bytes..];
                key_buf[..k.len()].copy_from_slice(k);
            }
        }

        // write values, possibly performing some copy optimizations
        if let Some(fixed_value_length) = self.fixed_value_length {
            let fixed_value_length = fixed_value_length.get() as usize;

            let self_values_sz = self.children as usize * fixed_value_length;
            let self_data_buf = self.data_buf();
            let self_values_buf =
                &self_data_buf[self_data_buf.len() - self_values_sz..];

            let ret_values_sz = ret.children as usize * fixed_value_length;
            let ret_data_buf = ret.data_buf_mut();
            let ret_dbl = ret_data_buf.len();
            let ret_values_buf = &mut ret_data_buf[ret_dbl - ret_values_sz..];

            let prelude = index * fixed_value_length;
            ret_values_buf[..prelude]
                .copy_from_slice(&self_values_buf[..prelude]);

            let item_end = prelude
                + if new_item.is_some() { fixed_value_length } else { 0 };

            if let Some((_, v)) = new_item {
                ret_values_buf[prelude..item_end].copy_from_slice(v);
            }

            let remaining_items =
                (children as usize) - (index + if replace { 0 } else { 1 });

            let ret_prologue_start = item_end;
            let ret_prologue_end =
                item_end + (remaining_items * fixed_value_length);

            let self_prologue_end =
                (self.children as usize) * fixed_value_length;
            let self_prologue_start =
                self_prologue_end - (remaining_items * fixed_value_length);

            ret_values_buf[ret_prologue_start..ret_prologue_end]
                .copy_from_slice(
                    &self_values_buf[self_prologue_start..self_prologue_end],
                );
        } else {
            for idx in 0..index {
                let v = self.index_value(idx);
                let mut value_buf = ret.value_buf_for_offset_mut(idx);
                let varint_bytes =
                    varint::serialize_into(v.len() as u64, value_buf);
                value_buf = &mut value_buf[varint_bytes..];
                value_buf[..v.len()].copy_from_slice(v);
            }

            if let Some((_, v)) = new_item {
                let mut value_buf = ret.value_buf_for_offset_mut(index);
                let varint_bytes =
                    varint::serialize_into(v.len() as u64, value_buf);
                value_buf = &mut value_buf[varint_bytes..];
                value_buf[..v.len()].copy_from_slice(v);
            }

            let start = index + if replace { 1 } else { 0 };

            for idx in start..self.children as usize {
                let self_idx = idx;
                let ret_idx = if replace {
                    if new_item.is_some() {
                        idx
                    } else {
                        idx - 1
                    }
                } else {
                    idx + 1
                };
                let v = self.index_value(self_idx);
                let mut value_buf = ret.value_buf_for_offset_mut(ret_idx);
                // println!("3 writing value {:?} at {:?}", v, value_buf.as_ptr());
                let varint_bytes =
                    varint::serialize_into(v.len() as u64, value_buf);
                value_buf = &mut value_buf[varint_bytes..];
                value_buf[..v.len()].copy_from_slice(v);
            }
        }

        testing_assert!(
            ret.is_sorted(),
            "after stitching item {:?} replace: {} index: {} \
            into node {:?}, ret is not sorted: {:?}",
            new_item,
            replace,
            index,
            self,
            ret
        );

        if let Some((k, v)) = new_item {
            assert_eq!(k, ret.index_key(index));
            assert_eq!(v, ret.index_value(index));
        } else if index < ret.len() {
            assert_ne!(self.index_key(index), ret.index_key(index));
        }

        ret
    }

    fn remove_index(&self, index: usize) -> Node {
        log::trace!("removing index {} for node {:?}", index, self);
        assert!(self.len() > index);
        self.stitch(index, None, true)
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Node {
        assert!(!self.merging);
        assert!(self.merging_child.is_none());

        let index = if let Err(prospective_offset) = self.find(key) {
            prospective_offset
        } else {
            panic!(
                "trying to insert key into node that already contains that key"
            );
        };

        self.stitch(index, Some((key, value)), false)
    }

    fn replace(&self, index: usize, value: &[u8]) -> Node {
        assert!(!self.merging);
        assert!(self.merging_child.is_none());

        // possibly short-circuit more expensive node recreation logic
        if self.index_value(index).len() == value.len() {
            let mut ret = self.clone();
            let requires_varint = ret.fixed_value_length.is_none();
            let mut value_buf = ret.value_buf_for_offset_mut(index);
            if requires_varint {
                // skip the varint bytes, which will be unchanged
                let varint_bytes =
                    usize::try_from(varint::size(value.len() as u64)).unwrap();
                value_buf = &mut value_buf[varint_bytes..];
            }

            value_buf[..value.len()].copy_from_slice(value);

            testing_assert!(
                ret.is_sorted(),
                "after replacing in-place item {:?} index: {} \
                into node {:?}, ret is not sorted: {:?}",
                value,
                index,
                self,
                ret
            );

            return ret;
        }

        self.stitch(index, Some((self.index_key(index), value)), true)
    }

    /*
        let removed_key = self.index_key(index);
        let removed_value = self.index_value(index);

        let mut offset_shift = 0;

        let removed_key_bytes = if self.fixed_key_length.is_some() {
            removed_key.len()
        } else {
            let removed_key_bytes = removed_key.len()
                + varint::size(removed_key.len() as u64) as usize;
            offset_shift += removed_key_bytes;
            removed_key_bytes
        };

        let removed_value_bytes = if self.fixed_value_length.is_some() {
            removed_value.len()
        } else {
            let removed_value_bytes = removed_value.len()
                + varint::size(removed_value.len() as u64) as usize;
            offset_shift += removed_value_bytes;
            removed_value_bytes
        };

        let new_sz = self.0.len()
            - self.offset_bytes as usize
            - removed_key_bytes
            - removed_value_bytes
            - size_of::<Header>();

        // allocate node and set header info
        let mut ret = Node(ManuallyDrop::new(aligned_boxed_slice(new_sz)));
        *ret.header_mut() = *self.header();
        ret.probation_ops_remaining =
            self.probation_ops_remaining.saturating_sub(1);
        ret.rewrite_generations = self.rewrite_generations;
        ret.children -= 1;

        // set lo and hi keys
        ret.lo_mut().copy_from_slice(self.lo());
        if let Some(ref mut hi_buf) = ret.hi_mut() {
            hi_buf.copy_from_slice(self.hi().unwrap());
        }

        // set offsets, properly shifted after index
        if ret.offset_bytes > 0 {
            // just copy the offsets before the index
            let start = usize::try_from(ret.lo_len).unwrap()
                + usize::try_from(ret.hi_len).unwrap()
                + size_of::<Header>();
            let end = start + (index * ret.offset_bytes as usize);

            ret.0[start..end].copy_from_slice(&self.0[start..end]);

            if ret.children > 0 {
                for i in (index + 1)..ret.children as usize {
                    // shift the old index down
                    let old_offset = self.offset(i + 1);
                    let shifted_offset = old_offset - offset_shift;
                    ret.set_offset(i, shifted_offset);
                }
            }
        }

        if ret.fixed_key_length.is_none() && ret.fixed_value_length.is_none() {
            // just iterate over keys and values
            for (mut idx, (k, v)) in self.iter().enumerate() {
                if idx == index {
                    continue;
                }
                if idx > index {
                    idx -= 1;
                }
                // skip the removed index and shift all other indices down by one
                let mut key_buf = ret.key_buf_for_offset_mut(idx);
                if self.fixed_key_length.is_none() {
                    let varint_bytes =
                        varint::serialize_into(k.len() as u64, key_buf);
                    key_buf = &mut key_buf[varint_bytes..];
                }
                key_buf[..k.len()].copy_from_slice(k);

                let mut value_buf = ret.value_buf_for_offset_mut(idx);
                if self.fixed_value_length.is_none() {
                    let varint_bytes =
                        varint::serialize_into(v.len() as u64, value_buf);
                    value_buf = &mut value_buf[varint_bytes..];
                }
                value_buf[..v.len()].copy_from_slice(v);
            }
        }

        testing_assert!(ret.is_sorted());

        ret
    }
    */

    fn weighted_split_point(&self) -> usize {
        let bits_set = self.activity_sketch.count_ones() as usize;

        if bits_set == 0 {
            // this shouldn't happen often, but it could happen
            // if we burn through our probation_ops_remaining
            // with just removals and no inserts, which don't tick
            // the activity sketch.
            return self.len() / 2;
        }

        let mut weighted_count = 0_usize;
        for bit in 0..8 {
            if (1 << bit) & self.activity_sketch != 0 {
                weighted_count += bit + 1;
            }
        }
        let average_bit = weighted_count / bits_set;
        (average_bit * self.children as usize / 8).min(self.len() - 1).max(1)
    }

    pub(crate) fn split(&self) -> (Node, Node) {
        assert!(self.len() >= 2);
        assert!(!self.merging);
        assert!(self.merging_child.is_none());

        let split_point = self.weighted_split_point();

        let left_max = self.index_key(split_point - 1);
        let right_min = self.index_key(split_point);

        // see if we can reduce the splitpoint length to reduce
        // the number of bytes that end up in index nodes
        let splitpoint_length = right_min.len();
        /*
            if self.is_index {
            right_min.len();
        } else {
            // we can only perform suffix truncation when
            // choosing the split points for leaf nodes.
            // split points bubble up into indexes, but
            // an important invariant is that for indexes
            // the first item always matches the lo key,
            // otherwise ranges would be permanently
            // inaccessible by falling into the gap
            // during a split.
            right_min
                .iter()
                .zip(left_max.iter())
                .take_while(|(a, b)| a == b)
                .count()
                + 1
        };
        */

        let untruncated_split_key = self.index_key(split_point);

        let possibly_truncated_split_key =
            &untruncated_split_key[..splitpoint_length];

        let split_key = self.prefix_decode(possibly_truncated_split_key);

        if untruncated_split_key.len() != possibly_truncated_split_key.len() {
            log::trace!(
                "shaved off {} bytes for split key",
                untruncated_split_key.len()
                    - possibly_truncated_split_key.len()
            );
        }

        // prefix encoded length can only grow or stay the same
        let additional_left_prefix = self.lo()[self.prefix_len as usize..]
            .iter()
            .zip(split_key[self.prefix_len as usize..].iter())
            .take((std::u8::MAX - self.prefix_len) as usize)
            .take_while(|(a, b)| a == b)
            .count();

        let additional_right_prefix = if let Some(hi) = self.hi() {
            split_key[self.prefix_len as usize..]
                .iter()
                .zip(hi[self.prefix_len as usize..].iter())
                .take((std::u8::MAX - self.prefix_len) as usize)
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            0
        };

        let left_items: Vec<_> = self
            .iter()
            .take(split_point)
            .map(|(k, v)| (&k[additional_left_prefix..], v))
            .collect();

        let right_items: Vec<_> = self
            .iter()
            .skip(split_point)
            .map(|(k, v)| (&k[additional_right_prefix..], v))
            .collect();

        let mut left = Node::new(
            self.lo(),
            Some(&split_key),
            self.prefix_len + u8::try_from(additional_left_prefix).unwrap(),
            self.is_index,
            self.next,
            &left_items,
        );

        left.rewrite_generations = self.rewrite_generations;

        let mut right = Node::new(
            &split_key,
            self.hi(),
            self.prefix_len + u8::try_from(additional_right_prefix).unwrap(),
            self.is_index,
            self.next,
            &right_items,
        );

        right.rewrite_generations = self.rewrite_generations;
        right.next = self.next;
        right.probation_ops_remaining =
            u8::try_from((self.len() / 2).min(std::u8::MAX as usize)).unwrap();

        log::trace!(
            "splitting node {:?} into left: {:?} and right: {:?}",
            self,
            left,
            right
        );

        testing_assert!(left.is_sorted());
        testing_assert!(right.is_sorted());

        (left, right)
    }

    pub(crate) fn receive_merge(&self, other: &Node) -> Node {
        assert_eq!(self.hi(), Some(other.lo()));
        assert_eq!(self.is_index, other.is_index);
        assert!(!self.merging);
        assert!(self.merging_child.is_none());

        let extended_keys: Vec<_>;
        let items: Vec<_> = if self.prefix_len == other.prefix_len {
            self.iter().chain(other.iter()).collect()
        } else if self.prefix_len > other.prefix_len {
            extended_keys = self
                .iter_keys()
                .map(|k| {
                    prefix::reencode(
                        self.prefix(),
                        k,
                        other.prefix_len as usize,
                    )
                })
                .collect();
            let left_items =
                extended_keys.iter().map(AsRef::as_ref).zip(self.iter_values());
            left_items.chain(other.iter()).collect()
        } else {
            // self.prefix_len < other.prefix_len
            extended_keys = other
                .iter_keys()
                .map(|k| {
                    prefix::reencode(
                        other.prefix(),
                        k,
                        self.prefix_len as usize,
                    )
                })
                .collect();
            let right_items = extended_keys
                .iter()
                .map(AsRef::as_ref)
                .zip(other.iter_values());
            self.iter().chain(right_items).collect()
        };

        let mut ret = Node::new(
            self.lo(),
            other.hi(),
            self.prefix_len.min(other.prefix_len),
            self.is_index,
            other.next,
            &*items,
        );

        ret.rewrite_generations =
            self.rewrite_generations.min(other.rewrite_generations);

        testing_assert!(ret.is_sorted());

        ret
    }

    pub(crate) fn should_split(&self) -> bool {
        let size_check = if cfg!(any(test, feature = "lock_free_delays")) {
            self.len() > 4
        } else if self.is_index {
            self.0.len() > 128 * 1024 && self.len() > 1
        } else {
            /*
            let threshold = match self.rewrite_generations {
                0 => 24 * 1024,
                1 => {
                    //println!("1, sz: {}", self.0.len());
                    64 * 1024
                }
                other => {
                    //println!("{}, sz: {}", other, self.0.len());
                    128 * 1024
                }
            };
            */
            let threshold = 2048;
            self.0.len() > threshold && self.len() > 1
        };

        let safety_checks = self.merging_child.is_none() && !self.merging;

        safety_checks && size_check
    }

    pub(crate) fn should_merge(&self) -> bool {
        let size_check = if cfg!(any(test, feature = "lock_free_delays")) {
            self.len() < 2
        } else if self.is_index {
            self.0.len() < 32 * 1024
        } else {
            /*
            let threshold = match self.rewrite_generations {
                0 => 10 * 1024,
                1 => 30 * 1024,
                other => {
                    /*
                    println!(
                        "merge {}, sz: {}, {} {} {}",
                        other,
                        self.0.len(),
                        !self.merging,
                        self.merging_child.is_none(),
                        self.probation_ops_remaining
                    );
                    */
                    64 * 1024
                }
            };
            */
            let threshold = 512;
            self.0.len() < threshold
        };

        let safety_checks = self.merging_child.is_none()
            && !self.merging
            && self.probation_ops_remaining == 0;

        safety_checks && size_check
    }

    fn header(&self) -> &Header {
        unsafe { &*(self.0.as_ptr() as *mut Header) }
    }

    fn header_mut(&mut self) -> &mut Header {
        unsafe { &mut *(self.0.as_mut_ptr() as *mut Header) }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn rss(&self) -> u64 {
        self.0.len() as u64
    }

    pub(crate) fn len(&self) -> usize {
        usize::from(self.children)
    }

    pub(crate) fn contains_key(&self, key: &[u8]) -> bool {
        self.find(key).is_ok()
    }

    fn find(&self, key: &[u8]) -> Result<usize, usize> {
        let mut size = self.len();
        if size == 0 || key < self.index_key(0) {
            return Err(0);
        }
        let mut base = 0_usize;
        while size > 1 {
            let half = size / 2;
            let mid = base + half;
            // mid is always in [0, size), that means mid is >= 0 and < size.
            // mid >= 0: by definition
            // mid < size: mid = size / 2 + size / 4 + size / 8 ...
            let l = self.index_key(mid);
            let cmp = crate::fastcmp(l, key);
            base = if cmp == Greater { base } else { mid };
            size -= half;
        }
        // base is always in [0, size) because base <= mid.
        let l = self.index_key(base);
        let cmp = crate::fastcmp(l, key);

        if cmp == Equal {
            Ok(base)
        } else {
            Err(base + (cmp == Less) as usize)
        }
    }

    pub(crate) fn can_merge_child(&self, pid: u64) -> bool {
        self.merging_child.is_none()
            && !self.merging
            && self.iter_index_pids().any(|p| p == pid)
    }

    pub(crate) fn index_next_node(&self, key: &[u8]) -> (usize, u64) {
        assert!(key >= self.lo());
        if let Some(hi) = self.hi() {
            assert!(hi > key);
        }
        assert!(self.is_index);
        log::trace!("index_next_node for key {:?} on node {:?}", key, self);
        let idx = match self.find(&key[self.prefix_len as usize..]) {
            Ok(idx) => idx,
            Err(idx) => idx - 1,
        };
        (idx, self.index_pid(idx))
    }

    pub(crate) fn parent_split(&self, at: &[u8], to: u64) -> Option<Node> {
        assert!(self.is_index, "tried to attach a ParentSplit to a Leaf Node");

        let encoded_sep = &at[self.prefix_len as usize..];
        if self.contains_key(encoded_sep) {
            log::debug!(
                "parent_split skipped because \
                parent already contains child with key {:?} \
                at split point due to deep race",
                at
            );
            return None;
        }

        Some(self.insert(encoded_sep, &to.to_le_bytes()))
    }

    pub(crate) fn iter_keys(
        &self,
    ) -> impl Iterator<Item = &[u8]> + ExactSizeIterator + DoubleEndedIterator
    {
        (0..self.len()).map(move |idx| self.index_key(idx))
    }

    pub(crate) fn iter_index_pids(
        &self,
    ) -> impl '_ + Iterator<Item = u64> + ExactSizeIterator + DoubleEndedIterator
    {
        assert!(self.is_index);
        self.iter_values().map(move |pid_bytes| {
            u64::from_le_bytes(pid_bytes.try_into().unwrap())
        })
    }

    pub(crate) fn iter_values(
        &self,
    ) -> impl Iterator<Item = &[u8]> + ExactSizeIterator + DoubleEndedIterator
    {
        (0..self.len()).map(move |idx| self.index_value(idx))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.iter_keys().zip(self.iter_values())
    }

    pub(crate) fn lo(&self) -> &[u8] {
        let start = size_of::<Header>();
        let end = start + usize::try_from(self.lo_len).unwrap();
        &self.0[start..end]
    }

    fn lo_mut(&mut self) -> &mut [u8] {
        let start = size_of::<Header>();
        let end = start + usize::try_from(self.lo_len).unwrap();
        &mut self.0[start..end]
    }

    pub(crate) fn hi(&self) -> Option<&[u8]> {
        let start = usize::try_from(self.lo_len).unwrap() + size_of::<Header>();
        let end = start + usize::try_from(self.hi_len).unwrap();
        if start == end {
            None
        } else {
            Some(&self.0[start..end])
        }
    }

    fn hi_mut(&mut self) -> Option<&mut [u8]> {
        let start = usize::try_from(self.lo_len).unwrap() + size_of::<Header>();
        let end = start + usize::try_from(self.hi_len).unwrap();
        if start == end {
            None
        } else {
            Some(&mut self.0[start..end])
        }
    }

    pub(crate) fn index_key(&self, idx: usize) -> &[u8] {
        assert!(
            idx < self.len(),
            "index {} is not less than internal length of {}",
            idx,
            self.len()
        );

        let offset_sz = self.children as usize * self.offset_bytes as usize;
        let keys_buf = &self.data_buf()[offset_sz..];
        let key_buf = {
            match (self.fixed_key_length, self.fixed_value_length) {
                (Some(k_sz), Some(_)) | (Some(k_sz), None) => {
                    &keys_buf[idx * usize::try_from(k_sz.get()).unwrap()..]
                }
                (None, Some(_)) | (None, None) => {
                    // find offset for key or combined kv offset
                    let offset = self.offset(idx);
                    &keys_buf[offset..]
                }
            }
        };

        let (start, end) = if let Some(fixed_key_length) = self.fixed_key_length
        {
            (0, usize::try_from(fixed_key_length.get()).unwrap())
        } else {
            let (key_len, varint_sz) = varint::deserialize(key_buf).unwrap();
            let start = varint_sz;
            let end = start + usize::try_from(key_len).unwrap();
            (start, end)
        };

        &key_buf[start..end]
    }

    pub(crate) fn index_value(&self, idx: usize) -> &[u8] {
        assert!(
            idx < self.len(),
            "index {} is not less than internal length of {}",
            idx,
            self.len()
        );

        let buf = self.value_buf_for_offset(idx);

        let (start, end) =
            if let Some(fixed_value_length) = self.fixed_value_length {
                (0, usize::try_from(fixed_value_length.get()).unwrap())
            } else {
                let (value_len, varint_sz) = varint::deserialize(buf).unwrap();
                let start = varint_sz;
                let end = start + usize::try_from(value_len).unwrap();
                (start, end)
            };

        &buf[start..end]
    }

    pub(crate) fn index_pid(&self, idx: usize) -> u64 {
        assert!(self.is_index);
        u64::from_le_bytes(self.index_value(idx).try_into().unwrap())
    }

    /// `node_kv_pair` returns either existing (node/key, value) pair or
    /// (node/key, none) where a node/key is node level encoded key.
    pub(crate) fn node_kv_pair<'a>(
        &'a self,
        key: &'a [u8],
    ) -> (&'a [u8], Option<&[u8]>, usize) {
        assert!(key >= self.lo());
        if let Some(hi) = self.hi() {
            assert!(key < hi);
        }

        let suffix = &key[self.prefix_len as usize..];

        let search = self.find(suffix);

        match search {
            Ok(idx) => (self.index_key(idx), Some(self.index_value(idx)), idx),
            Err(idx) => {
                let encoded_key = &key[self.prefix_len as usize..];
                let encoded_val = None;
                (encoded_key, encoded_val, idx)
            }
        }
    }

    pub(crate) fn contains_upper_bound(&self, bound: &Bound<IVec>) -> bool {
        if let Some(hi) = self.hi() {
            match bound {
                Bound::Excluded(bound) if hi >= &*bound => true,
                Bound::Included(bound) if hi > &*bound => true,
                _ => false,
            }
        } else {
            true
        }
    }

    pub(crate) fn contains_lower_bound(
        &self,
        bound: &Bound<IVec>,
        is_forward: bool,
    ) -> bool {
        let lo = self.lo();
        match bound {
            Bound::Excluded(bound)
                if lo < &*bound || (is_forward && *bound == lo) =>
            {
                true
            }
            Bound::Included(bound) if lo <= &*bound => true,
            Bound::Unbounded if !is_forward => self.hi().is_none(),
            _ => lo.is_empty(),
        }
    }

    fn prefix_decode(&self, key: &[u8]) -> IVec {
        prefix::decode(self.prefix(), key)
    }

    fn prefix_encode<'a>(&self, key: &'a [u8]) -> &'a [u8] {
        assert!(self.lo() <= key);
        if let Some(hi) = self.hi() {
            assert!(
                hi > key,
                "key being encoded {:?} >= self.hi {:?}",
                key,
                hi
            );
        }

        &key[self.prefix_len as usize..]
    }

    fn prefix(&self) -> &[u8] {
        &self.lo()[..self.prefix_len as usize]
    }

    pub(crate) fn successor(
        &self,
        bound: &Bound<IVec>,
    ) -> Option<(IVec, IVec)> {
        assert!(!self.is_index);

        // This encoding happens this way because
        // keys cannot be lower than the node's lo key.
        let predecessor_key = match bound {
            Bound::Unbounded => self.prefix_encode(self.lo()),
            Bound::Included(b) | Bound::Excluded(b) => {
                let max = std::cmp::max(&**b, self.lo());
                self.prefix_encode(max)
            }
        };

        let search = self.find(predecessor_key);

        let start = match search {
            Ok(start) => start,
            Err(start) if start < self.len() => start,
            _ => return None,
        };

        for (idx, k) in self.iter_keys().skip(start).enumerate() {
            match bound {
                Bound::Excluded(b) if b[self.prefix_len as usize..] == *k => {
                    // keep going because we wanted to exclude
                    // this key.
                    continue;
                }
                _ => {}
            }
            let decoded_key = self.prefix_decode(k);
            return Some((decoded_key, self.index_value(start + idx).into()));
        }

        None
    }

    pub(crate) fn predecessor(
        &self,
        bound: &Bound<IVec>,
    ) -> Option<(IVec, IVec)> {
        assert!(!self.is_index);

        // This encoding happens this way because
        // the rightmost (unbounded) node has
        // a hi key represented by the empty slice
        let successor_key = match bound {
            Bound::Unbounded => {
                if let Some(hi) = self.hi() {
                    Some(IVec::from(self.prefix_encode(hi)))
                } else {
                    None
                }
            }
            Bound::Included(b) => Some(IVec::from(self.prefix_encode(b))),
            Bound::Excluded(b) => {
                // we use manual prefix encoding here because
                // there is an assertion in `prefix_encode`
                // that asserts the key is within the node,
                // and maybe `b` is above the node.
                let encoded = &b[self.prefix_len as usize..];
                Some(IVec::from(encoded))
            }
        };

        let search = if let Some(successor_key) = successor_key {
            self.find(&*successor_key)
        } else if self.is_empty() {
            Err(0)
        } else {
            Ok(self.len() - 1)
        };

        let end = match search {
            Ok(end) => end,
            Err(end) if end > 0 => end - 1,
            _ => return None,
        };

        for (idx, k) in self.iter_keys().take(end + 1).enumerate().rev() {
            match bound {
                Bound::Excluded(b)
                    if b.len() >= self.prefix_len as usize
                        && b[self.prefix_len as usize..] == *k =>
                {
                    // keep going because we wanted to exclude
                    // this key.
                    continue;
                }
                _ => {}
            }
            let decoded_key = self.prefix_decode(k);

            return Some((decoded_key, self.index_value(idx).into()));
        }
        None
    }

    #[cfg(feature = "testing")]
    fn is_sorted(&self) -> bool {
        if self.len() <= 1 {
            return true;
        }

        for i in 0..self.len() - 1 {
            if self.index_key(i) >= self.index_key(i + 1) {
                log::error!(
                    "key {:?} at index {} >= key {:?} at index {}",
                    self.index_key(i),
                    i,
                    self.index_key(i + 1),
                    i + 1
                );
                return false;
            }
            /*
            println!(
                "key {:?} at index {} < key {:?} at index {}",
                self.index_key(i),
                i,
                self.index_key(i + 1),
                i + 1
            );
            */
        }

        true
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use quickcheck::{Arbitrary, Gen};

    use super::*;

    #[test]
    fn simple() {
        let mut ir = Node::new(
            &[1],
            Some(&[7]),
            0,
            true,
            None,
            &[
                (&[1], &42_u64.to_le_bytes()),
                (&[6, 6, 6], &66_u64.to_le_bytes()),
            ],
        );
        ir.next = Some(NonZeroU64::new(5).unwrap());
        format!("this is for miri to run the format code: {:#?}", ir);
        assert_eq!(ir.index_next_node(&[1]).1, 42);
        assert_eq!(ir.index_next_node(&[2]).1, 42);
        assert_eq!(ir.index_next_node(&[6]).1, 42);
        assert_eq!(ir.index_next_node(&[6, 6, 6, 6, 6]).1, 66);
    }

    impl Arbitrary for Node {
        fn arbitrary<G: Gen>(g: &mut G) -> Node {
            let lo: Vec<u8> = Arbitrary::arbitrary(g);
            let hi: Vec<u8> = Arbitrary::arbitrary(g);

            let children: BTreeMap<Vec<u8>, Vec<u8>> = Arbitrary::arbitrary(g);

            let children_ref: Vec<(&[u8], &[u8])> = children
                .iter()
                .map(|(k, v)| (k.as_ref(), v.as_ref()))
                .collect();
            Node::new(&lo, Some(&hi), 0, false, None, &children_ref)
        }
    }

    fn prop_indexable(
        lo: Vec<u8>,
        hi: Vec<u8>,
        children: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> bool {
        let children_ref: Vec<(&[u8], &[u8])> =
            children.iter().map(|(k, v)| (k.as_ref(), v.as_ref())).collect();
        let ir = Node::new(&lo, Some(&hi), 0, false, None, &children_ref);

        assert_eq!(ir.children as usize, children_ref.len());

        for (idx, (k, v)) in children_ref.iter().enumerate() {
            assert_eq!(ir.index_key(idx), *k);
            let value = ir.index_value(idx);
            assert_eq!(
                value, *v,
                "expected value index {} to have value {:?} but instead it was {:?}",
                idx, *v, value,
            );
        }
        true
    }

    quickcheck::quickcheck! {
        #[cfg_attr(miri, ignore)]
        fn indexable(lo: Vec<u8>, hi: Vec<u8>, children: BTreeMap<Vec<u8>, Vec<u8>>) -> bool {
            prop_indexable(lo, hi, children.into_iter().collect())
        }
    }

    #[test]
    fn node_bug_00() {
        // postmortem: offsets were not being stored, and the slot buf was not
        // being considered correctly while writing or reading values in
        // shared slots.
        assert!(prop_indexable(
            vec![],
            vec![],
            vec![(vec![], vec![]), (vec![1], vec![1]),]
        ));
    }

    #[test]
    fn node_bug_01() {
        // postmortem: hi and lo keys were not properly being accounted in the
        // inital allocation
        assert!(prop_indexable(vec![], vec![0], vec![],));
    }
}

mod prefix {
    use crate::IVec;

    pub(crate) fn empty() -> &'static [u8] {
        &[]
    }

    pub(crate) fn reencode(
        old_prefix: &[u8],
        old_encoded_key: &[u8],
        new_prefix_length: usize,
    ) -> IVec {
        old_prefix
            .iter()
            .chain(old_encoded_key.iter())
            .skip(new_prefix_length)
            .copied()
            .collect()
    }

    pub(crate) fn decode(old_prefix: &[u8], old_encoded_key: &[u8]) -> IVec {
        let mut decoded_key =
            Vec::with_capacity(old_prefix.len() + old_encoded_key.len());
        decoded_key.extend_from_slice(old_prefix);
        decoded_key.extend_from_slice(old_encoded_key);

        IVec::from(decoded_key)
    }
}
