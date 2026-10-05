//! Qoder 每日签到（活动平台 `sash` 一族接口，**双区域通用**）。
//!
//! ── 上游长什么样（2026-09-27 在真实账号上实测）────────────────
//! 每日权益以**活动（campaign）**形式下发，走 `sash` 一族接口：
//!
//! ```text
//! GET  {openapi}/sash/api/v1/me/campaigns?forceRefresh=true
//!   → {showCampaign, claimable, campaigns:[{campaignId, campaignKey,
//!        actionType, claimStatus, startAt, endAt,
//!        benefit:{kind:"CREDITS", amount:100,
//!                 validity:{mode:"RELATIVE_DAYS", days:30}}}]}
//! POST {openapi}/sash/api/v1/me/campaigns/{campaignId}/claim   body {}
//!   → {status:"CLAIMED", replayed, benefit:{...},
//!      claimedAt, grantedAt, expiresAt}      // 三个时间都是 ISO 字符串
//! ```
//!
//! 实测（中国版 Pro Trial 账号 `act-20260923-339`）：
//!   - 活动窗口 `startAt=1790474400 → endAt=1790560740`，即**当天 10:00 → 次日 09:59**
//!     （UTC+8），与官方客户端「每天 10:00 刷新」一致；
//!   - 领取返回 `{"status":"CLAIMED","replayed":false,"benefit":{...,"amount":100}}`，
//!     紧接着查 `/api/v2/quota/usage`，`addOnQuota` 从「上游不返回该键」变成
//!     `{total:100, remaining:100, unit:"credits"}` —— **签到额度落在 `addOnQuota`
//!     这个桶里**（余额面板「资源包」那一行，见 `balance.rs`）；
//!   - **领取后活动行不消失**，`claimStatus` 由 `CLAIMABLE` 变 `CLAIMED`。
//!     所以「今天已领取」直接从活动列表读，不需要本地记账；
//!   - 顶层的 `claimable` 标志**不能当判据**：领完之后它仍是 `true`（同账号还有一条
//!     可领的促销活动），必须按 `actionType` 逐行筛。
//!
//! ── 为什么不走 legacy 的 daily-check-in ─────────────────────
//! `GET /sash/api/v1/me/daily-check-in/status` 的 `status` 恒为 `DISABLED`
//! （国际版该路径直接 404），streak 与累计全 0；它的 claim 兄弟端点已被上游
//! 全局停用（对未领取的日子也回 409 且不发积分）。因此这里**只读活动列表**，
//! 一次都不碰 legacy —— 两个地区都是。
//!
//! ── 双区域为什么同一条链路 ─────────────────────────────────
//! 活动平台对两个地区**一视同仁**：列表与领取的路径、请求头（`cosy-clienttype`
//! 仍是门控，见 [`super::endpoints::SASH_CLIENT_TYPE`]）、响应字段完全一致，
//! 只差 openapi 主机名。多个参考实现交叉印证：cpa 系插件的国际版签到自 v0.8.18
//! 起就走这条链路（`qoder.com` 网页「活动」页用的正是这两个调用），中国版在
//! legacy 全局停用（2026-09-21 前后）后也切了过来；社区实现普遍把这套流程写成
//! 双区域通用、实测 `openapi.qoder.sh` 与 `openapi.qoder.com.cn` 均 200。
//! `billing::checkin::supports_checkin` 相应把国际版放进了 edition 白名单。
//!
//! 有没有活动可领是**账号侧**状态：免费档账号（两个地区都有实测）的活动列表里
//! 只有 `VIEW_DETAILS` 促销、没有可领的 `CLAIM_BENEFIT` 行。这种情况返回一条
//! 中性结果（`success:false` + 说明），不是错误 —— 把「今天没得领」报成失败
//! 会让用户以为接口坏了。
//!
//! ── 活动列表的「可见性」门控（排障先看这里）────────────────
//! 上游对请求不合规的反应**不是报错**，而是 HTTP 200 + 列表缺行甚至全空，
//! 与「账号今天没活动」在 HTTP 层无法区分：
//!   - `UA: Qoder` / `cosy-clienttype: 10` / `cosy-version` 三个头**缺任何一个**
//!     → 空活动列表（参考实现实测；见 `endpoints::SASH_CLIENT_VERSION`）；
//!   - 国际版活动平台还按**官方客户端的机器身份**过滤（UMID，由官方客户端
//!     组件生成、约 50 分钟刷新）：伪造全套 `cosy-machine*` 六头会被判定为
//!     非官方客户端、把 CLAIMABLE 行整条滤掉；一个都不发则**国际版可能整包
//!     不下发**（`showCampaign:false`）。本网关不发机器头 —— 对中国版无影响，
//!     对国际版是已知限制。
//!
//! 因此「没领到」分三种口径：`showCampaign:false` 报「未认可客户端身份」；
//! 列表有行但没有可领的签到活动报账号侧状态；verbose 日志始终打一份活动行
//! 摘要供排查。
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本文件零 unwrap/expect/panic，取值全走 Option 链。

use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::ApiResponse;
use crate::server::core::proxies::ResolvedProxy;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::credentials::Credentials;
use super::{auth, endpoints, refresh};

/// 活动类型：可领取权益（促销类活动是 `VIEW_DETAILS`，不是签到，不能领）
const ACTION_CLAIM_BENEFIT: &str = "CLAIM_BENEFIT";
/// 活动状态：可领取
const STATUS_CLAIMABLE: &str = "CLAIMABLE";
/// 活动状态：已领取
const STATUS_CLAIMED: &str = "CLAIMED";
/// 领取响应的 `result` 字段（上游在并发/重放时会给这个值）
const RESULT_ALREADY_CLAIMED: &str = "ALREADY_CLAIMED";
/// 活动状态：被风控拦下（原因看 `failureCode`）
const STATUS_BLOCKED: &str = "BLOCKED";
/// 被拦原因：同一自然人名下的其它账号已领（服务端按人去重）
const BLOCKED_SAME_PERSON: &str = "SAME_PERSON_ALREADY_CLAIMED";
/// 上游没给 `benefit.amount` 时的缺省奖励额（实测就是 100）
const DEFAULT_REWARD: f64 = 100.0;

/// Qoder 的每日签到（双区域通用，按账号凭证里的地区自动选站点）。
///
/// 返回 `{success, msg, ...}` —— 与 WorkBuddy / 小浣熊 / AutoClaw 三家同形状
/// （见 `billing::checkin::claim_result`），于是汇总、日志与界面三处不需要
/// 为第四家再加分支。
///
/// `success` 的口径是「**本次真的领到了**」：
///   - 领到 → `true`，`msg` 带金额与有效期，另给 `rewardPoints`（界面显示「本次 +N」）
///     与 `unit`（单位是 credits，不是积分）；
///   - 今天已领 / 没有活动 / 同人已领 → `false` + `msg`，其中前两种（含同人已领，
///     服务端按人去重，见 `same_person_blocked`）另带 `alreadyCompleted: true`，
///     `billing::checkin` 据此落 `checkinAt`（行上的按钮变成「已签到」）；
///     「没有活动」不带它 —— 不能把「今天没得领」记成已签到。
pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let mut credentials = refresh::ensure_fresh(store, account_id, false).await?;
    let (record, _) = refresh::snapshot(store, account_id)?;
    let proxy = auth::account_proxy(&record)?;

    let mut response = fetch_campaigns(&credentials, proxy.as_ref()).await?;
    if response.status == 401 && credentials.can_refresh() {
        credentials = refresh::ensure_fresh(store, account_id, true).await?;
        response = fetch_campaigns(&credentials, proxy.as_ref()).await?;
    }
    let list = auth::payload(response, "签到活动查询")?;
    let rows: Vec<Value> = list
        .get("campaigns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // 活动列表的可见性是排障的关键：上游对「头不齐 / 客户端身份不认可」的响应
    // 是 HTTP 200 + 列表缺行甚至全空（见模块头「可见性门控」），与「账号今天
    // 没活动」在 HTTP 层无法区分。verbose 打一份行摘要（普通用户不打扰），
    // 用户开了运行日志 verbose 就能看清这次上游到底回了什么。
    let summary = rows
        .iter()
        .map(|row| {
            format!(
                "{}:{}",
                row.get("actionType").and_then(Value::as_str).unwrap_or("?"),
                row.get("claimStatus").and_then(Value::as_str).unwrap_or("?"),
            )
        })
        .collect::<Vec<_>>()
        .join("、");
    logging::verbose(
        "[Qoder]",
        &format!(
            "签到活动查询（{}）：showCampaign={} 活动行[{}]",
            credentials.region.id(),
            list.get("showCampaign")
                .map(|value| value.to_string())
                .unwrap_or_else(|| "缺".to_string()),
            if summary.is_empty() { "无" } else { summary.as_str() },
        ),
    );

    // `showCampaign` 被明确置 false：服务端没有认可本次请求的客户端身份，
    // 活动整包不下发 —— 这不是「账号没活动」，单独成一种口径（见 no_claim）。
    let identity_rejected = matches!(list.get("showCampaign"), Some(Value::Bool(false)));

    let mut claimable: Option<&Value> = None;
    let mut claimed = false;
    for row in &rows {
        if row.get("actionType").and_then(Value::as_str) != Some(ACTION_CLAIM_BENEFIT) {
            continue;
        }
        match row.get("claimStatus").and_then(Value::as_str) {
            Some(STATUS_CLAIMABLE) if claimable.is_none() => claimable = Some(row),
            Some(STATUS_CLAIMED) => claimed = true,
            _ => {}
        }
    }

    let Some(target) = claimable else {
        return Ok(no_claim(claimed, identity_rejected));
    };
    let campaign_id = target.get("campaignId").and_then(Value::as_str).unwrap_or("");
    if !is_safe_campaign_id(campaign_id) {
        return Err(GatewayError::with_status(502, "Qoder 签到活动标识缺失或形态异常，无法领取"));
    }

    let url = format!(
        "{}{}/{}/claim",
        credentials.region.open_api(),
        endpoints::CAMPAIGNS_PATH,
        campaign_id
    );
    let mut claim = claim_campaign(&url, &credentials, proxy.as_ref()).await?;
    // 上游并发/重放时给 409 + `{"result":"ALREADY_CLAIMED"}`（参考实现实测），
    // 那不是失败，是「今天已经领过了」。
    if !claim.ok && (claim.status == 409 || result_of(&claim).as_deref() == Some(RESULT_ALREADY_CLAIMED))
    {
        return Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        }));
    }
    if claim.status == 401 && credentials.can_refresh() {
        credentials = refresh::ensure_fresh(store, account_id, true).await?;
        claim = claim_campaign(&url, &credentials, proxy.as_ref()).await?;
    }
    let body = auth::payload(claim, "签到领取")?;
    // 国际版活动页可能把领取结果包一层 `{data:{…}}` 信封（见 `claim_body`），
    // 之后统一从业务体里读字段。
    let body = claim_body(&body);

    // 同一自然人名下已有账号领过今天这份：服务端按人去重。这等价于「这个账号
    // 今天领不到了」，归一成 `alreadyCompleted` 让批量 / 定时签到停手，而不是
    // 每轮都白打一次上游（也不能装作签到成功 —— 额度并没有到账）。
    if same_person_blocked(body) {
        return Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "同一身份下的其它账号今日已领取",
        }));
    }
    if body.get("status").and_then(Value::as_str) != Some(STATUS_CLAIMED) {
        let status = body.get("status").and_then(Value::as_str).unwrap_or("未知");
        return Ok(json!({ "success": false, "msg": format!("签到未完成（上游状态 {status}）") }));
    }
    if body.get("replayed").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        }));
    }

    let reward = body
        .get("benefit")
        .and_then(|benefit| benefit.get("amount"))
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_REWARD);
    let valid_days = body
        .get("benefit")
        .and_then(|benefit| benefit.get("validity"))
        .and_then(|validity| validity.get("days"))
        .and_then(Value::as_f64);
    // 文案不重复「签到成功」：调用方 `claim_result` 已经用它作日志前缀
    // （「账号 X: 签到成功（…）」），这里只说领到了什么。
    let msg = match valid_days {
        Some(days) if days > 0.0 => format!("获得 {reward} credits，{days} 天有效"),
        _ => format!("获得 {reward} credits"),
    };
    Ok(json!({
        "success": true,
        "msg": msg,
        "rewardPoints": reward,
        "unit": "credits",
    }))
}

/// 活动列表（`forceRefresh` 拿实时 `claimStatus`）。
async fn fetch_campaigns(
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let url = format!(
        "{}{}{}",
        credentials.region.open_api(),
        endpoints::CAMPAIGNS_PATH,
        endpoints::CAMPAIGNS_QUERY
    );
    auth::request(
        "GET",
        &url,
        None,
        &endpoints::sash_headers(&credentials.access_token),
        proxy,
    ).await
}

/// 领取一个活动。请求体是空对象（抓包确认：无参数），`Origin` 指向该地区门户
/// （参考实现只在 POST 上带它）。
async fn claim_campaign(
    url: &str,
    credentials: &Credentials,
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    let mut headers = endpoints::sash_headers(&credentials.access_token);
    headers.push(("content-type".to_string(), "application/json".to_string()));
    headers.push(("origin".to_string(), credentials.region.open_api().to_string()));
    auth::request("POST", url, Some(&json!({})), &headers, proxy).await
}

/// campaignId 能否安全拼进 URL 路径段（见 [`claim_campaign`]）：非空，且只含
/// 上游实测出现过的字符（字母 / 数字 / 连字符 / 下划线）。列表响应被污染时
/// 宁可报 502，也不把 `..`、`/` 这类字符带进请求路径。
fn is_safe_campaign_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// 领取响应的业务体。
///
/// 国际版活动页的两条调用存在**信封形态**：领取结果可能包一层
/// `{"data": {…}}`（网页端把整包交给渲染层，`status` / `replayed` / `benefit`
/// 都在 `data` 里），也可能是裸载荷（中国版抓包如此）。
///
/// 下钻的判据是「顶层没有本链路要读的任何键」：裸载荷顶层必有
/// `status` / `replayed` / `benefit` / `result` 之一（`result` 是并发/重放标记，
/// 409 重放响应的顶层可能只有它，见 [`result_of`]），原样返回 —— 哪怕它恰好
/// 带一个对象型的 `data` 字段（与领取结果无关）也不会被误下钻；信封形态顶层
/// 没有这些键，才进 `data` 找。两种形态用同一个读法。
fn claim_body(payload: &Value) -> &Value {
    const READ_KEYS: [&str; 4] = ["status", "replayed", "benefit", "result"];
    if READ_KEYS.iter().any(|key| payload.get(key).is_some()) {
        return payload;
    }
    payload.get("data").filter(|value| value.is_object()).unwrap_or(payload)
}

/// 「同一自然人名下已有账号领过今天这份」：
/// `status == BLOCKED` 且 `failureCode == SAME_PERSON_ALREADY_CLAIMED`。
/// 其它拦截原因（真风控）不能冒充它 —— 那些该按失败如实透出。
fn same_person_blocked(body: &Value) -> bool {
    body.get("status").and_then(Value::as_str) == Some(STATUS_BLOCKED)
        && body.get("failureCode").and_then(Value::as_str) == Some(BLOCKED_SAME_PERSON)
}

/// 领取响应的 `result` 字段（并发/重放标记），拿不到给 None。
///
/// 信封形态下 `result` 也可能包在 `data` 里，两层都看（见 [`claim_body`]）。
fn result_of(response: &ApiResponse) -> Option<String> {
    let payload = response.payload.as_ref()?;
    let value = claim_body(payload).get("result")?.as_str()?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 「这次没领到」的结果：区分「今天已领」「服务端没下发活动」与「没有可领的活动」。
///
/// ── 为什么「身份未认可」要单独成一种口径 ──────────────────
/// 国际版活动平台按官方客户端的机器身份（UMID）过滤请求，本网关不带机器头时
/// 服务端可能整包不下发活动（`showCampaign:false`，见模块头「可见性门控」）。
/// 把它报成「今天没有活动」会让用户白等一天再回来问；如实说明是已知限制、
/// 并指一条官方客户端的核对路径，才是这条消息该干的活。
fn no_claim(claimed: bool, identity_rejected: bool) -> Value {
    if claimed {
        return json!({
            "success": false,
            "alreadyCompleted": true,
            "msg": "今日已领取",
        });
    }
    if identity_rejected {
        return json!({
            "success": false,
            "msg": "服务端未认可本次客户端身份、未下发活动列表（国际版活动平台按官方客户端的机器身份过滤，这是已知限制）；可在官方客户端的活动页确认该账号今天是否有可领活动",
        });
    }
    // 没有可领的活动是账号侧状态（免费档账号两个地区都有实测：活动列表里只有
    // VIEW_DETAILS 促销），明确说「没有可领的」而不是报错。
    json!({
        "success": false,
        "msg": "当前没有可领取的签到活动",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 领取响应两种形态都要能读：国际版活动页可能包 `{data:{…}}` 信封，
    /// 中国版抓包是裸载荷 —— `claim_body` 统一成后者。
    #[test]
    fn claim_body_unwraps_data_envelope_and_passes_bare_payload() {
        let enveloped = json!({"data": {"status": "CLAIMED", "replayed": false}});
        let body = claim_body(&enveloped);
        assert_eq!(body.get("status").and_then(Value::as_str), Some("CLAIMED"));

        // 裸载荷原样透传；`data` 存在但不是对象（null / 数组）也不下钻
        let bare = json!({"status": "CLAIMED", "data": null});
        let body = claim_body(&bare);
        assert_eq!(body.get("status").and_then(Value::as_str), Some("CLAIMED"));
        let array_enveloped = json!({"status": "CLAIMED", "data": [1]});
        assert_eq!(
            claim_body(&array_enveloped).get("status").and_then(Value::as_str),
            Some("CLAIMED"),
        );

        // 顶层已有要读的键时不下钻：即使 `data` 是对象，也不丢顶层字段
        let mixed = json!({"status": "CLAIMED", "data": {"note": "unrelated"}});
        assert_eq!(claim_body(&mixed).get("status").and_then(Value::as_str), Some("CLAIMED"));
        // 信封顶层可以带无关键（code / msg），照样下钻
        let noisy_envelope = json!({"code": 0, "msg": "ok", "data": {"status": "CLAIMED"}});
        assert_eq!(
            claim_body(&noisy_envelope).get("status").and_then(Value::as_str),
            Some("CLAIMED"),
        );
    }

    /// campaignId 直接拼进 URL 路径段，字符集必须收紧：空串与路径字符
    /// （`..`、`/`、空格等）一律拒绝，走 502 出口，不进请求。
    #[test]
    fn campaign_id_charset_is_pinned() {
        assert!(is_safe_campaign_id("act-20260923-339"));
        assert!(is_safe_campaign_id("camp_01"));
        assert!(!is_safe_campaign_id(""));
        assert!(!is_safe_campaign_id("../../other"));
        assert!(!is_safe_campaign_id("has space"));
        assert!(!is_safe_campaign_id("带中文"));
    }

    /// 并发/重放标记 `result` 在两种形态下都要能读到。
    #[test]
    fn result_of_reads_bare_and_enveloped_payloads() {
        let bare = ApiResponse { status: 200, ok: true, payload: Some(json!({"result": "ALREADY_CLAIMED"})) };
        assert_eq!(result_of(&bare).as_deref(), Some(RESULT_ALREADY_CLAIMED));
        let enveloped = ApiResponse {
            status: 200,
            ok: true,
            payload: Some(json!({"data": {"result": "ALREADY_CLAIMED"}})),
        };
        assert_eq!(result_of(&enveloped).as_deref(), Some(RESULT_ALREADY_CLAIMED));
        // 拿不到 / 空串给 None
        let missing = ApiResponse { status: 200, ok: true, payload: Some(json!({"status": "CLAIMED"})) };
        assert_eq!(result_of(&missing), None);
        let empty = ApiResponse { status: 200, ok: true, payload: Some(json!({"result": "  "})) };
        assert_eq!(result_of(&empty), None);
    }

    /// 同人已领的判定必须两个字段都对上：其它拦截原因（真风控）与普通状态
    /// 都不能冒充「同人已领」—— 冒充会让该重试的失败被吞掉。
    #[test]
    fn same_person_blocked_requires_both_fields() {
        assert!(same_person_blocked(&json!({
            "status": "BLOCKED",
            "failureCode": "SAME_PERSON_ALREADY_CLAIMED",
        })));
        assert!(!same_person_blocked(&json!({
            "status": "BLOCKED",
            "failureCode": "RISK_CONTROL",
        })));
        assert!(!same_person_blocked(&json!({"status": "CLAIMED"})));
    }

    /// 「没领到」的结果三态：已领带 `alreadyCompleted`；身份未认可与「没活动」
    /// 都不带它（不能落 `checkinAt`），但文案要分开 —— 前者是网关侧已知限制，
    /// 后者是账号侧状态，混成一句会让用户往错误的方向排查。
    #[test]
    fn no_claim_distinguishes_already_claimed_from_no_campaign() {
        let claimed = no_claim(true, false);
        assert_eq!(claimed.get("alreadyCompleted"), Some(&json!(true)));

        let none = no_claim(false, false);
        assert_eq!(none.get("alreadyCompleted"), None);
        assert_eq!(
            none.get("msg").and_then(Value::as_str),
            Some("当前没有可领取的签到活动"),
        );

        let rejected = no_claim(false, true);
        assert_eq!(rejected.get("alreadyCompleted"), None);
        let msg = rejected.get("msg").and_then(Value::as_str).unwrap_or("");
        assert!(msg.contains("未认可") && msg.contains("机器身份"));
        assert!(!msg.contains("没有可领取的签到活动"));
    }
}
