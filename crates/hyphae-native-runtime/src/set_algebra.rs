// SPDX-License-Identifier: Apache-2.0

//! Bounded request and result contracts for native binary-set algebra.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

/// Maximum caller positions accepted by one set-algebra request.
pub const MAX_SET_ALGEBRA_KEYS: usize = 64;

/// Maximum complete result members admitted by one set-algebra request.
pub const MAX_SET_ALGEBRA_OUTPUT_MEMBERS: usize = 65_536;

/// Maximum member visits admitted by one set-algebra request.
pub const MAX_SET_ALGEBRA_VISITS: usize = 1_048_576;

/// Exact native set-algebra operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetAlgebraOperation {
    /// Members present in at least one input set.
    Union,
    /// Members present in every input set.
    Intersection,
    /// Members in the first input and absent from every later input.
    Difference,
}

/// Native set-algebra request validation or bounded-execution failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SetAlgebraError {
    /// The request has no input keys or exceeds its fixed position bound.
    #[error(
        "native set algebra contains {requested} key positions; expected 1..={MAX_SET_ALGEBRA_KEYS}"
    )]
    InvalidKeyCount {
        /// Rejected caller-supplied key-position count.
        requested: usize,
    },
    /// The complete-result member limit is zero or exceeds its hard bound.
    #[error(
        "native set algebra output limit {requested} is outside 1..={MAX_SET_ALGEBRA_OUTPUT_MEMBERS}"
    )]
    InvalidOutputLimit {
        /// Rejected caller-supplied result bound.
        requested: usize,
    },
    /// The member-visit limit is zero or exceeds its hard bound.
    #[error("native set algebra visit limit {requested} is outside 1..={MAX_SET_ALGEBRA_VISITS}")]
    InvalidVisitLimit {
        /// Rejected caller-supplied work bound.
        requested: usize,
    },
    /// Another complete result member would exceed the admitted bound.
    #[error("native set algebra exceeds its {maximum}-member complete-result limit")]
    OutputLimitExceeded {
        /// Maximum complete-result members admitted by the request.
        maximum: usize,
    },
    /// Another member visit would exceed the admitted work bound.
    #[error("native set algebra exceeds its {maximum}-visit work limit")]
    VisitLimitExceeded {
        /// Maximum member visits admitted by the request.
        maximum: usize,
    },
}

/// One checked, bounded native set-algebra request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetAlgebraRequest {
    operation: SetAlgebraOperation,
    keys: Vec<Vec<u8>>,
    output_member_limit: usize,
    visit_limit: usize,
}

impl SetAlgebraRequest {
    /// Validates one complete-result algebra request.
    ///
    /// # Errors
    ///
    /// Returns [`SetAlgebraError`] for zero or oversized key, output, or visit
    /// bounds.
    pub fn try_new(
        operation: SetAlgebraOperation,
        keys: Vec<Vec<u8>>,
        output_member_limit: usize,
        visit_limit: usize,
    ) -> Result<Self, SetAlgebraError> {
        validate_bounded_nonzero(keys.len(), MAX_SET_ALGEBRA_KEYS, |requested| {
            SetAlgebraError::InvalidKeyCount { requested }
        })?;
        validate_bounded_nonzero(
            output_member_limit,
            MAX_SET_ALGEBRA_OUTPUT_MEMBERS,
            |requested| SetAlgebraError::InvalidOutputLimit { requested },
        )?;
        validate_bounded_nonzero(visit_limit, MAX_SET_ALGEBRA_VISITS, |requested| {
            SetAlgebraError::InvalidVisitLimit { requested }
        })?;
        Ok(Self {
            operation,
            keys,
            output_member_limit,
            visit_limit,
        })
    }

    /// Returns the exact requested operation.
    pub const fn operation(&self) -> SetAlgebraOperation {
        self.operation
    }

    /// Returns caller key positions without deduplication.
    pub fn keys(&self) -> &[Vec<u8>] {
        &self.keys
    }

    /// Returns the maximum admitted complete-result cardinality.
    pub const fn output_member_limit(&self) -> usize {
        self.output_member_limit
    }

    /// Returns the maximum admitted member visits.
    pub const fn visit_limit(&self) -> usize {
        self.visit_limit
    }
}

/// One complete, ascending native set-algebra result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetAlgebraResult {
    members: Vec<Vec<u8>>,
    visited: usize,
}

impl SetAlgebraResult {
    pub(crate) const fn new(members: Vec<Vec<u8>>, visited: usize) -> Self {
        Self { members, visited }
    }

    /// Returns exact members in strictly ascending binary order.
    pub fn members(&self) -> &[Vec<u8>] {
        &self.members
    }

    /// Returns member visits consumed by this execution surface.
    pub const fn visited(&self) -> usize {
        self.visited
    }
}

pub(crate) fn evaluate_materialized_set_algebra(
    sets: &BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    request: &SetAlgebraRequest,
) -> Result<SetAlgebraResult, SetAlgebraError> {
    match request.operation() {
        SetAlgebraOperation::Union => materialized_union(sets, request),
        SetAlgebraOperation::Intersection => materialized_intersection(sets, request),
        SetAlgebraOperation::Difference => materialized_difference(sets, request),
    }
}

pub(crate) struct SetAlgebraExecution<'request> {
    request: &'request SetAlgebraRequest,
    members: BTreeSet<Vec<u8>>,
    visited: usize,
}

impl<'request> SetAlgebraExecution<'request> {
    pub(crate) const fn new(request: &'request SetAlgebraRequest) -> Self {
        Self {
            request,
            members: BTreeSet::new(),
            visited: 0,
        }
    }

    pub(crate) fn consume_visit(&mut self) -> Result<(), SetAlgebraError> {
        if self.visited == self.request.visit_limit() {
            return Err(SetAlgebraError::VisitLimitExceeded {
                maximum: self.request.visit_limit(),
            });
        }
        self.visited += 1;
        Ok(())
    }

    pub(crate) fn insert(&mut self, member: &[u8]) -> Result<(), SetAlgebraError> {
        if self.members.contains(member) {
            return Ok(());
        }
        if self.members.len() == self.request.output_member_limit() {
            return Err(SetAlgebraError::OutputLimitExceeded {
                maximum: self.request.output_member_limit(),
            });
        }
        self.members.insert(member.to_vec());
        Ok(())
    }

    pub(crate) fn finish(self) -> SetAlgebraResult {
        SetAlgebraResult::new(self.members.into_iter().collect(), self.visited)
    }
}

fn materialized_union(
    sets: &BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    request: &SetAlgebraRequest,
) -> Result<SetAlgebraResult, SetAlgebraError> {
    let mut execution = SetAlgebraExecution::new(request);
    for key in request.keys() {
        if let Some(members) = sets.get(key) {
            for member in members {
                execution.consume_visit()?;
                execution.insert(member)?;
            }
        }
    }
    Ok(execution.finish())
}

fn materialized_intersection(
    sets: &BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    request: &SetAlgebraRequest,
) -> Result<SetAlgebraResult, SetAlgebraError> {
    let mut sources = Vec::with_capacity(request.keys().len());
    for (position, key) in request.keys().iter().enumerate() {
        let Some(members) = sets.get(key) else {
            return Ok(SetAlgebraExecution::new(request).finish());
        };
        if members.is_empty() {
            return Ok(SetAlgebraExecution::new(request).finish());
        }
        sources.push((position, members));
    }
    let (source_position, source) = sources
        .into_iter()
        .min_by_key(|(position, members)| (members.len(), *position))
        .ok_or(SetAlgebraError::InvalidKeyCount { requested: 0 })?;
    let mut execution = SetAlgebraExecution::new(request);
    for member in source {
        execution.consume_visit()?;
        let mut present_everywhere = true;
        for (position, key) in request.keys().iter().enumerate() {
            if position == source_position {
                continue;
            }
            execution.consume_visit()?;
            if !sets
                .get(key)
                .is_some_and(|members| members.contains(member))
            {
                present_everywhere = false;
                break;
            }
        }
        if present_everywhere {
            execution.insert(member)?;
        }
    }
    Ok(execution.finish())
}

fn materialized_difference(
    sets: &BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    request: &SetAlgebraRequest,
) -> Result<SetAlgebraResult, SetAlgebraError> {
    let Some(first) = sets.get(&request.keys()[0]) else {
        return Ok(SetAlgebraExecution::new(request).finish());
    };
    let mut execution = SetAlgebraExecution::new(request);
    for member in first {
        execution.consume_visit()?;
        let mut subtracted = false;
        for key in &request.keys()[1..] {
            execution.consume_visit()?;
            if sets
                .get(key)
                .is_some_and(|members| members.contains(member))
            {
                subtracted = true;
                break;
            }
        }
        if !subtracted {
            execution.insert(member)?;
        }
    }
    Ok(execution.finish())
}

fn validate_bounded_nonzero<E>(
    value: usize,
    maximum: usize,
    error: impl FnOnce(usize) -> E,
) -> Result<(), E> {
    if value == 0 || value > maximum {
        Err(error(value))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SET_ALGEBRA_KEYS, MAX_SET_ALGEBRA_OUTPUT_MEMBERS, MAX_SET_ALGEBRA_VISITS,
        SetAlgebraError, SetAlgebraOperation, SetAlgebraRequest,
    };

    #[test]
    fn request_bounds_are_exact_and_preserve_positions() -> Result<(), Box<dyn std::error::Error>> {
        let request = SetAlgebraRequest::try_new(
            SetAlgebraOperation::Difference,
            vec![b"first".to_vec(), b"first".to_vec()],
            MAX_SET_ALGEBRA_OUTPUT_MEMBERS,
            MAX_SET_ALGEBRA_VISITS,
        )?;
        assert_eq!(request.keys(), [b"first".to_vec(), b"first".to_vec()]);
        assert!(matches!(
            SetAlgebraRequest::try_new(SetAlgebraOperation::Union, Vec::new(), 1, 1),
            Err(SetAlgebraError::InvalidKeyCount { requested: 0 })
        ));
        assert!(matches!(
            SetAlgebraRequest::try_new(
                SetAlgebraOperation::Union,
                vec![Vec::new(); MAX_SET_ALGEBRA_KEYS + 1],
                1,
                1
            ),
            Err(SetAlgebraError::InvalidKeyCount { .. })
        ));
        assert!(matches!(
            SetAlgebraRequest::try_new(
                SetAlgebraOperation::Intersection,
                vec![b"set".to_vec()],
                0,
                1
            ),
            Err(SetAlgebraError::InvalidOutputLimit { requested: 0 })
        ));
        assert!(matches!(
            SetAlgebraRequest::try_new(
                SetAlgebraOperation::Intersection,
                vec![b"set".to_vec()],
                MAX_SET_ALGEBRA_OUTPUT_MEMBERS + 1,
                1
            ),
            Err(SetAlgebraError::InvalidOutputLimit { .. })
        ));
        assert!(matches!(
            SetAlgebraRequest::try_new(
                SetAlgebraOperation::Intersection,
                vec![b"set".to_vec()],
                1,
                0
            ),
            Err(SetAlgebraError::InvalidVisitLimit { requested: 0 })
        ));
        assert!(matches!(
            SetAlgebraRequest::try_new(
                SetAlgebraOperation::Intersection,
                vec![b"set".to_vec()],
                1,
                MAX_SET_ALGEBRA_VISITS + 1
            ),
            Err(SetAlgebraError::InvalidVisitLimit { .. })
        ));
        Ok(())
    }
}
