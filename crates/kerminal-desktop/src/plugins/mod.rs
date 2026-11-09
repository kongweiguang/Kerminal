//! 外部插件的桌面组合入口；声明冻结早于配置扫描，运行服务晚于基础设施装配。
//! @author kongweiguang

pub mod arguments;
pub mod assembly;
pub mod dev_reload;
pub mod native_ui;
pub mod startup;
pub mod view_runtime;
