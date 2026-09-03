//! 与平台无关的核心业务模型。
//!
//! 本 crate 不得依赖 Win32、网络、数据库或具体 UI 框架，保证数据规则可以用快速单元测试验证。

pub mod activity;
pub mod layout;
pub mod official;
pub mod quota;
pub mod usage;
