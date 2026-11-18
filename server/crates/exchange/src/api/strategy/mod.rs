//! Strategy endpoints.
//!
//! * [`compile`] — `POST /api/strategy` and `POST /api/strategy/compile`: compile an
//!   IR document against the venue registry without deploying it (stateless),
//!   `GET /api/strategy/schema` (the document's JSON Schema), and the compile
//!   helper the deploy route shares.
//! * [`store`] — `POST/GET/PATCH/DELETE /api/accounts/{address}/strategies[/{id}]`
//!   (account-scoped CRUD: sub-account funding, the stored record, and the
//!   running deployment).
//! * [`template`] — `POST /api/accounts/{address}/templates/{id}/deploy`:
//!   deploy one side of a public template from its boot-compiled program.

pub mod compile;
pub mod store;
pub mod template;

pub use compile::{CompileRequest, CompileResponse, compile_for_venue, compile_strategy, get_strategy_schema};
pub use store::{
    StrategyDetail, StrategySummary, build_strategy_list, build_strategy_summary,
    create_strategy, delete_strategy, fund_strategy, get_strategy, list_strategies,
    update_strategy,
};
pub use template::{DeployTemplateRequest, deploy_template, deploy_template_side, deploy_template_side_with};
