//! Qoder 的地区与鉴权端点；地区不接受任意 URL。

use serde_json::Value;

use crate::server::errors::GatewayError;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Global,
    Cn,
}

impl Region {
    pub fn parse(value: &str) -> Result<Self, GatewayError> {
        match value.trim() {
            "" | "global" | "intl" => Ok(Self::Global),
            "cn" => Ok(Self::Cn),
            _ => Err(GatewayError::with_status(400, "Qoder 地区必须为 global（国际版）或 cn（中国版）")),
        }
    }

    pub fn from_payload(payload: &Value) -> Result<Self, GatewayError> {
        let mode = payload.get("mode").or_else(|| payload.get("edition"));
        match mode {
            None | Some(Value::Null) => Ok(Self::Global),
            Some(Value::String(value)) => Self::parse(value),
            _ => Err(GatewayError::with_status(400, "Qoder 地区必须是字符串")),
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Cn => "cn",
        }
    }

    pub fn edition(self) -> &'static str {
        match self {
            Self::Global => "intl",
            Self::Cn => "cn",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "国际版",
            Self::Cn => "中国版",
        }
    }

    pub fn open_api(self) -> &'static str {
        match self {
            Self::Global => "https://openapi.qoder.sh",
            Self::Cn => "https://openapi.qoder.com.cn",
        }
    }

    /// 网页门户基址（登录页、账号设置页所在的那台主机）。
    ///
    /// 与 [`Self::open_api`] 是两台不同的主机：门户负责「人看的页面」，
    /// openapi 负责「机器调的接口」，设备授权恰好两边都用（授权页在门户、
    /// 轮询在 openapi）。
    pub fn web_origin(self) -> &'static str {
        match self {
            Self::Global => "https://qoder.com",
            Self::Cn => "https://qoder.com.cn",
        }
    }

    /// 设备授权页的完整地址（含 [`DEVICE_LOGIN_PATH`]）。
    pub fn device_login_url(self) -> String {
        format!("{}{}", self.web_origin(), DEVICE_LOGIN_PATH)
    }

    /// 推理网关基址（`/algo/...` 这一族的根）。
    ///
    /// 它同时承载模型目录（`algo/api/v2/model/list`）与对话
    /// （`algo/api/v2/service/pro/sse/agent_chat_generation`）——
    /// 两者走同一套 COSY 签名（见 `cosy.rs`），所以共用这一个基址。
    /// **注意**：对话链路还有一台按令牌分流的主机，见 [`Self::inference_base`]。
    pub fn gateway(self) -> &'static str {
        match self {
            Self::Global => "https://api3.qoder.sh/",
            Self::Cn => "https://gateway.qoder.com.cn/",
        }
    }

    /// **对话链路**实际使用的推理基址：国际版的作业令牌要走 `api2`。
    ///
    /// ── 为什么按令牌前缀分流（这不是取舍，是上游的约束）──────────
    /// 国际版有两台推理主机：`api3.qoder.sh` 认设备流令牌（`dt-`），
    /// `api2.qoder.sh` 才认 PAT 换来的作业令牌（`jt-`）—— 把 `jt-` 打到
    /// api3 会被判「Login expired」（403）。官方 qodercli 也是这么分的：
    /// PAT 先换作业令牌、再走 api2。中国版只有一台网关
    /// （`gateway.qoder.com.cn`），两种令牌都收，不需要分流。
    ///
    /// 依据：9router 的 `qoderInferenceBase`（注释写明「Job-token (jt-...)
    /// traffic must hit api2.qoder.sh — api3 rejects jt- with "Login expired"」）、
    /// CLIProxyAPI 的 qoder2api 插件同款分流、OmniRoute 的 issue #4683。
    /// 官方文档也把 api1 / api2 / api3 三台主机都列为可连通主机。
    pub fn inference_base(self, access_token: &str) -> &'static str {
        match self {
            Self::Cn => "https://gateway.qoder.com.cn/",
            Self::Global => {
                if access_token.starts_with("jt-") {
                    "https://api2.qoder.sh/"
                } else {
                    "https://api3.qoder.sh/"
                }
            }
        }
    }
}

/// 设备授权页路径。**两站同名同参**：只有主机名不同（`qoder.com` / `qoder.com.cn`），
/// 实测 `challenge` / `challenge_method` / `machine_id` / `nonce` 四个 query 参数
/// 两站都认，未登录时都 302 到各自的 `/users/sign-in?oauth_callback=…`。
pub const DEVICE_LOGIN_PATH: &str = "/device/selectAccounts";
pub const DEVICE_POLL_PATH: &str = "/api/v1/deviceToken/poll";
pub const EXCHANGE_PATH: &str = "/api/v1/jobToken/exchange";
pub const USER_INFO_PATH: &str = "/api/v1/userinfo";
pub const USAGE_PATH: &str = "/api/v2/quota/usage";

/// 套餐查询（`GET`，返回 `plan_tier_name` 等）。与额度接口同在 openapi 主机上，
/// 用同一套头；余额面板的「套餐 Pro Trial」一行就来自这里。
pub const PLAN_PATH: &str = "/api/v2/user/plan";

/// 活动/签到接口（`sash` 一族）的路径前缀。
///
/// **与 openapi 同主机、但鉴权口径完全不同**：只要一个 `Bearer <dt->` 加
/// `cosy-clienttype: 10`，**不打 COSY 签名**（见 [`sash_headers`]）。
/// 每日签到的领取走 `GET {CAMPAIGNS_PATH}` 找 `actionType == "CLAIM_BENEFIT"`
/// 的活动、再 `POST {CAMPAIGNS_PATH}/{campaignId}/claim` 领取 —— 详情与证据见
/// `checkin.rs` 的模块头。
pub const CAMPAIGNS_PATH: &str = "/sash/api/v1/me/campaigns";
/// 活动列表的查询串：`forceRefresh=true` 让上游跳过缓存给实时的 `claimStatus`。
pub const CAMPAIGNS_QUERY: &str = "?forceRefresh=true";

/// `cosy-clienttype` 的值：桌面端（QoderWork）形态。
///
/// **这个头在门控返回内容**，不是可有可无的装饰：实测同一个账号
/// （中国版，2026-09-27）用 `Cosy-ClientType: 5`（IDE 形态）打活动接口，
/// 上游返回 `showCampaign:false` + 空活动列表；换成 `10` 才给出完整活动包
/// （含可领取的签到活动）。参考实现（CLIProxyAPI 的 qoder2api 插件、
/// CPA 的 qoder 插件）都用 10。
pub const SASH_CLIENT_TYPE: &str = "10";

/// `cosy-version` 的值：官方桌面端版本号。
///
/// **它与 clienttype 一样在门控活动列表**：参考实现实测 `UA: Qoder` /
/// `cosy-clienttype` / `cosy-version` 三个头**缺任何一个**，服务端不报错但
/// 返回**空活动列表**（qoder2api-hub 实测，2026-10）；cpa 系插件在国际版
/// （openapi.qoder.sh，2026-09-21 实测）也始终带着它（值 0.3.4，老一档的
/// 桌面端版本）。这里取较新的官方桌面端版本号；上游对旧值不拒绝（cpa 的
/// 0.3.4 一直可用），说明该头只要求「存在且像桌面端」，不校验具体值。
pub const SASH_CLIENT_VERSION: &str = "0.4.3";

/// `user-agent`：抓包确认的客户端标识（参考实现同值）
pub const SASH_USER_AGENT: &str = "Qoder";

/// 活动/签到接口的请求头：**最小束、无签名**。
///
/// 小写头名是照抄抓包结果（HTTP 头名大小写不敏感，但保持一致便于对拍）。
/// `user-agent` / `cosy-clienttype` / `cosy-version` 三个都在门控活动列表
/// （见 [`SASH_CLIENT_VERSION`]），**一个都不能少**。
pub fn sash_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".to_string(), format!("Bearer {token}")),
        ("cosy-clienttype".to_string(), SASH_CLIENT_TYPE.to_string()),
        ("cosy-version".to_string(), SASH_CLIENT_VERSION.to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("accept-language".to_string(), "zh-CN".to_string()),
        ("user-agent".to_string(), SASH_USER_AGENT.to_string()),
    ]
}

/// 设备流凭据（`dt-` / `drt-`）的续期端点，挂在 [`Region::open_api`] 上。
///
/// ── 为什么不是 `{center}/algo/api/v3/user/refresh_token` ──────────
/// 那条「认证中心」端点是给 PAT 换来的作业令牌（`jt-` / `jrt-`）用的，且要求
/// `appcode: cosy` + `Date` + `signature = md5("cosy&<secret>&<date>")` 这套签名头
/// （`center.qoder.sh` 与 `gateway.qoder.com.cn` 同名同签）。我们的设备流凭据走
/// 那条路会**连签名都没有**，被 WAF 直接丢掉 —— 实测两个地区都稳定返回
/// `403 {"errorMessage":"Request discarded","errorCode":"Forbidden"}`，与凭证
/// 有效性、地区选择都无关。补上签名头后同一请求会从 403 变成 400 业务校验，
/// 足以证明 403 来自缺签名而不是令牌本身。
///
/// 设备流凭据的续期在 **openapi 主机**上、且**不需要任何签名**（对照实现里
/// 只带 `content-type` / `accept` 两个头就能刷新成功）。
pub const DEVICE_REFRESH_PATH: &str = "/api/v1/deviceToken/refresh";

/// 作业令牌（`jrt-`，PAT 换来的那一族）的续期端点。
///
/// 与 [`DEVICE_REFRESH_PATH`] 同在 openapi 主机上、同样不要签名，只有路径不同。
/// 两者**不可互换**：把 `drt-` 交给作业令牌端点、或把 `jrt-` 交给设备端点，
/// 上游都按「令牌无效」拒绝。
pub const JOB_REFRESH_PATH: &str = "/api/v1/jobToken/refresh";

/// 续期请求的头：**不带任何签名，也不带 Authorization**。
///
/// 两条续期路径的鉴权凭据都在请求体里（`{"refresh_token": "…"}`），带旧令牌
/// 没有意义。多带 `Cosy-*` 也不会让 center 那一族接受（那需要的是 appcode 签名），
/// 因此这里只保留一个可辨识的 UA。
pub fn refresh_headers() -> Vec<(String, String)> {
    vec![("User-Agent".to_string(), "qoder-local-proxy".to_string())]
}

pub fn open_api_headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Cosy-Version".to_string(), "1.0.1".to_string()),
        ("Cosy-ClientType".to_string(), "5".to_string()),
        ("User-Agent".to_string(), "qoder-local-proxy".to_string()),
    ];
    if let Some(token) = token {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}
