use std::collections::HashMap;

use crate::error::DotsyncError;

#[derive(Debug, Clone)]
pub(crate) struct ScopeGraph {
    pub(crate) parents: HashMap<String, Vec<String>>,
    pub(crate) children: HashMap<String, Vec<String>>,
}

impl ScopeGraph {
    pub(crate) fn new(parents: HashMap<String, Vec<String>>) -> Result<Self, DotsyncError> {
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for scope in parents.keys() {
            children.entry(scope.clone()).or_default();
        }
        for (scope, scope_parents) in &parents {
            for parent in scope_parents {
                if !parents.contains_key(parent) {
                    return Err(DotsyncError::MissingParent {
                        scope: scope.clone(),
                        parent: parent.clone(),
                    });
                }
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(scope.clone());
            }
        }
        let graph = Self { parents, children };
        validate_scope_graph(&graph)?;
        Ok(graph)
    }
}

pub(crate) fn validate_scope_graph(graph: &ScopeGraph) -> Result<(), DotsyncError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Visiting,
        Visited,
    }

    fn visit(
        graph: &ScopeGraph,
        scope: &str,
        states: &mut HashMap<String, VisitState>,
    ) -> Result<(), DotsyncError> {
        if let Some(state) = states.get(scope) {
            return match state {
                VisitState::Visiting => Err(DotsyncError::ScopeCycle {
                    scope: scope.to_string(),
                }),
                VisitState::Visited => Ok(()),
            };
        }

        states.insert(scope.to_string(), VisitState::Visiting);
        if let Some(parents) = graph.parents.get(scope) {
            for parent in parents {
                visit(graph, parent, states)?;
            }
        }
        states.insert(scope.to_string(), VisitState::Visited);
        Ok(())
    }

    let mut states = HashMap::new();
    for scope in graph.parents.keys() {
        visit(graph, scope, &mut states)?;
    }
    Ok(())
}

pub(crate) fn scope_depth(
    graph: &ScopeGraph,
    scope: &str,
    memo: &mut HashMap<String, usize>,
) -> Result<usize, DotsyncError> {
    if let Some(depth) = memo.get(scope) {
        return Ok(*depth);
    }
    let parents = graph
        .parents
        .get(scope)
        .ok_or_else(|| DotsyncError::InvalidScope {
            scope: scope.to_string(),
        })?;
    let depth = if parents.is_empty() {
        0
    } else {
        parents
            .iter()
            .map(|parent| scope_depth(graph, parent, memo))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0)
            + 1
    };
    memo.insert(scope.to_string(), depth);
    Ok(depth)
}
