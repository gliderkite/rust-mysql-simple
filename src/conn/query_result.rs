// Copyright (c) 2020 rust-mysql-simple contributors
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

pub use mysql_common::proto::{Binary, Text};

use mysql_common::packets::OkPacket;

use std::{borrow::Cow, marker::PhantomData, sync::Arc};

use crate::{conn::ConnMut, Column, Conn, Error, Result, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Or<A, B> {
    A(A),
    B(B),
}

/// Result set kind.
pub trait Protocol: 'static + Send + Sync {
    fn next(conn: &mut Conn, columns: Arc<[Column]>) -> Result<Option<Row>>;
}

impl Protocol for Text {
    fn next(conn: &mut Conn, columns: Arc<[Column]>) -> Result<Option<Row>> {
        conn.next_text(columns)
    }
}

impl Protocol for Binary {
    fn next(conn: &mut Conn, columns: Arc<[Column]>) -> Result<Option<Row>> {
        conn.next_bin(columns)
    }
}

/// State of a result set iterator.
#[derive(Debug)]
enum SetIteratorState {
    /// Iterator is in a non-empty set.
    InSet(Arc<[Column]>),
    /// Iterator is in an empty set.
    InEmptySet(OkPacket<'static>),
    /// Iterator is in an errored result set.
    Errored(Error),
    /// Next result set isn't handled.
    OnBoundary,
    /// No more result sets.
    Done,
}

impl SetIteratorState {
    fn ok_packet(&self) -> Option<&OkPacket<'_>> {
        if let Self::InEmptySet(ref ok) = self {
            Some(ok)
        } else {
            None
        }
    }

    fn columns(&self) -> Option<&Arc<[Column]>> {
        if let Self::InSet(ref cols) = self {
            Some(cols)
        } else {
            None
        }
    }
}

impl From<Vec<Column>> for SetIteratorState {
    fn from(columns: Vec<Column>) -> Self {
        Self::InSet(columns.into())
    }
}

impl From<OkPacket<'static>> for SetIteratorState {
    fn from(ok_packet: OkPacket<'static>) -> Self {
        Self::InEmptySet(ok_packet)
    }
}

impl From<Error> for SetIteratorState {
    fn from(err: Error) -> Self {
        Self::Errored(err)
    }
}

impl From<Or<Vec<Column>, OkPacket<'static>>> for SetIteratorState {
    fn from(or: Or<Vec<Column>, OkPacket<'static>>) -> Self {
        match or {
            Or::A(cols) => Self::from(cols),
            Or::B(ok) => Self::from(ok),
        }
    }
}

/// Response to a query or statement execution.
///
/// It is an iterator:
/// *   over result sets (via `Self::next_set`)
/// *   over rows of a current result set (via `Iterator` impl)
#[derive(Debug)]
pub struct QueryResult<'c, 't, 'tc, T: crate::prelude::Protocol> {
    conn: ConnMut<'c, 't, 'tc>,
    state: SetIteratorState,
    set_index: usize,
    protocol: PhantomData<T>,
}

impl<'c, 't, 'tc, T: crate::prelude::Protocol> QueryResult<'c, 't, 'tc, T> {
    fn from_state(
        conn: ConnMut<'c, 't, 'tc>,
        state: SetIteratorState,
    ) -> QueryResult<'c, 't, 'tc, T> {
        QueryResult {
            conn,
            state,
            set_index: 0,
            protocol: PhantomData,
        }
    }

    pub(crate) fn new(
        conn: ConnMut<'c, 't, 'tc>,
        meta: Or<Vec<Column>, OkPacket<'static>>,
    ) -> QueryResult<'c, 't, 'tc, T> {
        Self::from_state(conn, meta.into())
    }

    /// Updates state with the next result set, if any.
    ///
    /// Returns `false` if there is no next result set.
    ///
    /// **Requires:** `self.state == OnBoundary`
    fn handle_next(&mut self) {
        if cfg!(debug_assertions) {
            if let SetIteratorState::OnBoundary = self.state {
            } else {
                panic!("self.state == OnBoundary");
            }
        }

        if self.conn.more_results_exists() {
            match self.conn.handle_result_set() {
                Ok(meta) => self.state = meta.into(),
                Err(err) => self.state = err.into(),
            }
            self.set_index += 1;
        } else {
            self.state = SetIteratorState::Done;
        }
    }

    /// Returns an iterator over the current result set.
    pub fn next_set<'d>(&'d mut self) -> Option<Result<ResultSet<'c, 't, 'tc, 'd, T>>> {
        use SetIteratorState::*;

        match &self.state {
            OnBoundary | Done => {
                debug_assert!(
                    !self.conn.more_results_exists(),
                    "the next state must be handled by the Iterator::next"
                );

                None
            }
            Errored(_) => None,
            _ => Some(Ok(ResultSet {
                set_index: self.set_index,
                inner: self,
            })),
        }
    }

    /// Returns the number of affected rows for the current result set.
    pub fn affected_rows(&self) -> u64 {
        self.state
            .ok_packet()
            .map(|ok| ok.affected_rows())
            .unwrap_or_default()
    }

    /// Returns the last insert id for the current result set.
    pub fn last_insert_id(&self) -> Option<u64> {
        self.state
            .ok_packet()
            .map(|ok| ok.last_insert_id())
            .unwrap_or_default()
    }

    /// Returns the warnings count for the current result set.
    pub fn warnings(&self) -> u16 {
        self.state
            .ok_packet()
            .map(|ok| ok.warnings())
            .unwrap_or_default()
    }

    /// [Info] for the current result set.
    ///
    /// Will be empty if not defined.
    ///
    /// [Info]: http://dev.mysql.com/doc/internals/en/packet-OK_Packet.html
    pub fn info_ref(&self) -> &[u8] {
        self.state
            .ok_packet()
            .and_then(|ok| ok.info_ref())
            .unwrap_or_default()
    }

    /// [Info] for the current result set.
    ///
    /// Will be empty if not defined.
    ///
    /// [Info]: http://dev.mysql.com/doc/internals/en/packet-OK_Packet.html
    pub fn info_str(&self) -> Cow<str> {
        self.state
            .ok_packet()
            .and_then(|ok| ok.info_str())
            .unwrap_or_else(|| "".into())
    }

    /// Returns columns of the current result rest.
    pub fn columns(&self) -> SetColumns {
        SetColumns {
            inner: self.state.columns().map(Into::into),
        }
    }
}

impl<'c, 't, 'tc, T: crate::prelude::Protocol> Drop for QueryResult<'c, 't, 'tc, T> {
    fn drop(&mut self) {
        while self.next_set().is_some() {}
    }
}

#[derive(Debug)]
pub struct ResultSet<'a, 'b, 'c, 'd, T: crate::prelude::Protocol> {
    set_index: usize,
    inner: &'d mut QueryResult<'a, 'b, 'c, T>,
}

impl<'a, 'b, 'c, T: crate::prelude::Protocol> std::ops::Deref for ResultSet<'a, 'b, 'c, '_, T> {
    type Target = QueryResult<'a, 'b, 'c, T>;

    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}

impl<T: crate::prelude::Protocol> Iterator for ResultSet<'_, '_, '_, '_, T> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.set_index == self.inner.set_index {
            self.inner.next()
        } else {
            None
        }
    }
}

impl<T: crate::prelude::Protocol> Iterator for QueryResult<'_, '_, '_, T> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        use SetIteratorState::*;

        let state = std::mem::replace(&mut self.state, OnBoundary);

        match state {
            InSet(cols) => match T::next(&mut *self.conn, cols.clone()) {
                Ok(Some(row)) => {
                    self.state = InSet(cols.clone());
                    Some(Ok(row))
                }
                Ok(None) => {
                    self.handle_next();
                    None
                }
                Err(e) => {
                    self.handle_next();
                    Some(Err(e))
                }
            },
            InEmptySet(_) => {
                self.handle_next();
                None
            }
            Errored(err) => {
                self.handle_next();
                Some(Err(err))
            }
            OnBoundary => None,
            Done => {
                self.state = Done;
                None
            }
        }
    }
}

impl<T: crate::prelude::Protocol> Drop for ResultSet<'_, '_, '_, '_, T> {
    fn drop(&mut self) {
        while self.next().is_some() {}
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetColumns<'a> {
    inner: Option<&'a Arc<[Column]>>,
}

impl<'a> SetColumns<'a> {
    /// Returns an index of a column by its name.
    pub fn column_index<U: AsRef<str>>(&self, name: U) -> Option<usize> {
        let name = name.as_ref().as_bytes();
        self.inner
            .as_ref()
            .and_then(|cols| cols.iter().position(|col| col.name_ref() == name))
    }

    pub fn as_ref(&self) -> &[Column] {
        self.inner
            .as_ref()
            .map(|cols| &(*cols)[..])
            .unwrap_or(&[][..])
    }
}
