//! 引导页的「推荐插件」：装 dsh 时顺手装进 profile 的那几个。
//!
//! 只收「装了就有用、不装也只是少个面板」的插件，默认全选 —— 一路点「下一步」
//! 就能得到完整工作区，不用装完再进插件市场找。取消勾选后不再安装，选择存进
//! [`crate::settings::Settings::plugins`]。
//!
//! 安装动作由 dsh 自己做（`dsh plugin --profile <name> add <pkg>...`，内部就是
//! 在该 profile 目录里跑 pnpm add），所以这里只负责「装哪些、叫什么、怎么说」。

/// 一条推荐项：包名 + 界面标题 + 一句话说明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecommendedPlugin {
    /// npm 包名，也是 `dsh plugin add` 的参数。
    pub name: &'static str,
    /// 界面上的标题（给人看的短名）。
    pub title: &'static str,
    /// 一句话说明：这插件到底让用户多出什么能力。
    pub blurb: &'static str,
}

/// 推荐清单。顺序即界面顺序，越常用的越靠前。
pub const RECOMMENDED: &[RecommendedPlugin] = &[
    RecommendedPlugin {
        name: "suveng-dsh-plugin-memory",
        title: "记忆系统",
        blurb: "跨会话记住偏好与结论，下次对话自动带回来",
    },
    RecommendedPlugin {
        name: "billion-context",
        title: "上下文压缩",
        blurb: "长会话自动压缩历史，省 token 也不丢线索",
    },
    RecommendedPlugin {
        name: "suveng-dsh-plugin-mcp-manager",
        title: "MCP 管理",
        blurb: "在界面里增删改 MCP 服务器，不用手写配置",
    },
    RecommendedPlugin {
        name: "dsh-plugin-feishu",
        title: "飞书",
        blurb: "把飞书消息接进来，群聊里也能派任务",
    },
];

/// 全部推荐包名（默认勾选的就是这一份）。
#[must_use]
pub fn recommended_names() -> Vec<String> {
    RECOMMENDED
        .iter()
        .map(|item| item.name.to_owned())
        .collect()
}

/// 是不是我们推荐过的包。设置里只认这些名字，手改配置文件塞进来的
/// 任意包名不会在本机被悄悄 `pnpm add`。
#[must_use]
pub fn is_recommended(name: &str) -> bool {
    RECOMMENDED.iter().any(|item| item.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn recommended_names_are_unique_and_complete() {
        let names = recommended_names();
        assert_eq!(names.len(), RECOMMENDED.len());
        let unique: BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "推荐清单里有重复包名");
        for name in &names {
            assert!(is_recommended(name));
            // 只要是个规矩的 npm 包名即可（`billion-context` 这类中性名字也收）。
            assert!(
                !name.is_empty() && !name.contains(|c: char| c.is_whitespace() || c.is_uppercase()),
                "包名不像 npm 名：{name}"
            );
        }
    }

    #[test]
    fn every_entry_explains_itself() {
        for item in RECOMMENDED {
            assert!(!item.title.trim().is_empty());
            assert!(!item.blurb.trim().is_empty());
            assert!(item.name.trim() == item.name, "包名不该带空白");
        }
    }

    #[test]
    fn unknown_packages_are_rejected() {
        assert!(!is_recommended("left-pad"));
        assert!(!is_recommended(""));
    }
}
