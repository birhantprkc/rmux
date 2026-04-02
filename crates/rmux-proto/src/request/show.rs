use serde::{Deserialize, Serialize};

use crate::{HookName, OptionScopeSelector, ScopeSelector};

/// Request payload for `show-options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowOptionsRequest {
    /// The selected option scope.
    pub scope: OptionScopeSelector,
    /// Optional exact option name to display.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether output should contain only values.
    pub value_only: bool,
}

/// Request payload for `show-environment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowEnvironmentRequest {
    /// The selected environment scope.
    pub scope: ScopeSelector,
    /// Optional exact variable to display.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether only hidden variables should be displayed.
    #[serde(default)]
    pub hidden: bool,
    /// Whether output should use shell-export syntax.
    #[serde(default)]
    pub shell_format: bool,
}

/// Request payload for `show-hooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowHooksRequest {
    /// The selected hook scope.
    pub scope: ScopeSelector,
    /// Whether the query should use the window hook table.
    pub window: bool,
    /// Whether the query should use the pane hook table.
    pub pane: bool,
    /// The optional specific hook to display.
    pub hook: Option<HookName>,
}
