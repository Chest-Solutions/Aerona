//! The gateway connection, as one function.
//!
//! [`connect`] is what both `aerona-bot`'s `main` and the unified `aerona` binary call.
//! It owns the client, the background loops and the presence-on-shutdown dance, and it
//! returns: that is the contract a supervisor needs, because "the worker is unhealthy"
//! is only useful if something can hear it and decide what to do next.
//!
//! It never loops internally. Retrying in here would hide the reason behind a wall of
//! identical log lines and would make `--check` and `connect` disagree about what a
//! failure means.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serenity::all::{ClientBuilder, GatewayIntents, OnlineStatus};

use aerona_core::config::Config;
use aerona_core::error::{Error, Result};

use crate::events;
use crate::state::BotState;
use crate::tasks;

/// Seconds to let the offline presence frame leave before the socket does.
const PRESENCE_GRACE: Duration = Duration::from_secs(1);

/// Which gateway events to ask for. `GUILD_MEMBERS` and `MESSAGE_CONTENT` are the two
/// privileged intents: the first is what makes a join visible at all (and is on the
/// dashboard's checklist), the second is opt-in because it is the one a reviewer asks
/// about, and prefix commands are useless without it.
pub fn chosen_intents(cfg: &Config) -> GatewayIntents {
    let mut intents =
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::GUILD_MODERATION;
    intents |= GatewayIntents::GUILD_MEMBERS;
    if cfg.content_intent {
        intents |= GatewayIntents::MESSAGE_CONTENT;
    } else {
        tracing::warn!(
            "the MESSAGE_CONTENT intent is off: prefix commands and message scanning need it - set AERONA_CONTENT_INTENT=true and enable it in the developer portal"
        );
    }
    intents
}

/// The intent names, in the order the Discord portal lists them.
///
/// Deliberately spelled out instead of `format!("{i:?}")`: serenity's bitflags print a
/// *single* intent from `iter()` as `GatewayIntents(512)`, which tells an operator
/// nothing about what to tick in the developer portal - and this string is exactly the
/// `check` output they read while deciding that.
pub fn describe_intents(cfg: &Config) -> Vec<String> {
    let intents = chosen_intents(cfg);
    [
        ("GUILDS", GatewayIntents::GUILDS),
        ("GUILD_MODERATION", GatewayIntents::GUILD_MODERATION),
        ("GUILD_MEMBERS", GatewayIntents::GUILD_MEMBERS),
        ("GUILD_MESSAGES", GatewayIntents::GUILD_MESSAGES),
        ("MESSAGE_CONTENT", GatewayIntents::MESSAGE_CONTENT),
    ]
    .into_iter()
    .filter(|(_, flag)| intents.contains(*flag))
    .map(|(name, _)| name.to_string())
    .collect()
}

/// The bot half of `check`: intents and providers, with no network call. Whether the
/// token is *accepted* is a Discord fact, not a configuration fact, and an operator
/// must be able to check config on a box with no egress - `aerona doctor` is the
/// command that asks the network.
pub fn report(cfg: &Config) -> String {
    format!(
        "intents: {}\ntext provider: {}",
        describe_intents(cfg).join(", "),
        if cfg.sightengine.is_some() {
            "configured"
        } else {
            "not configured (local rules only)"
        }
    )
}

/// Connect, run until `shutdown` resolves or Discord gives up on us.
///
/// `Err` means "this worker is dead and needs a fresh attempt" - a bad token, a
/// rejected session, a shard set that could not be built. `Ok` means it was told to
/// stop. Either way the background loops are gone when this returns, so a supervisor may
/// drop this [`BotState`] and build a new one without leaking tasks.
pub async fn connect(
    state: Arc<BotState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    if state.cfg.token.expose().len() < 40 {
        return Err(Error::Config(
            "the bot token is too short to be real".into(),
        ));
    }

    let intents = chosen_intents(&state.cfg);
    let handler = events::Aerona::new(Arc::clone(&state));
    // The builder is itself awaited: serenity constructs the shard set (and therefore
    // asks Discord for the gateway URL and the bot's own application id) during
    // construction, which is why there is no separate `build()`.
    let mut client = ClientBuilder::new(state.cfg.token.expose(), intents)
        .event_handler(handler)
        .await
        .map_err(|e| Error::Config(format!("could not build the gateway client: {e}")))?;

    let loops = tasks::spawn(Arc::clone(&state));
    // `?intents` prints `GatewayIntents(519)`: serenity's bitflags `Debug` only names a
    // set when it is *not* a single flag, so the one line an operator reads when the bot
    // behaves oddly - "what did it actually ask Discord for" - was a number. Names, from
    // the same table `aerona check` prints.
    tracing::info!(
        version = aerona_core::VERSION,
        guilds = state.db.guilds().map(|g| g.len()).unwrap_or(0),
        intents = describe_intents(&state.cfg).join(" ").as_str(),
        "aerona-bot starting"
    );

    // The signal path is deliberately symmetrical with the API: SIGTERM means "stop
    // taking work", and the outbox rows that were claimed but not finished are left for
    // the next attempt, where the claim timeout requeues them.
    tokio::pin!(shutdown);
    let result = tokio::select! {
        res = client.start_autosharded() => {
            res.map_err(|e| Error::Config(format!("the gateway stopped: {e}")))
        }
        _ = &mut shutdown => {
            tracing::info!("stop signal: going offline and letting in-flight actions finish");
            if let Some(ctx) = state.ctx() {
                ctx.set_presence(None, OnlineStatus::Offline);
                tokio::time::sleep(PRESENCE_GRACE).await;
            }
            Ok(())
        }
    };

    for handle in loops {
        handle.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Real-shaped but inert: `Config` checks length and segment count so that a
    /// pasted invite link fails at boot instead of at the gateway.
    const TOKEN: &str = "MTk4MTA5NzA5NTA4Njk0MDgw.GsxABc.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let mut env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        env.entry("AERONA_PORT".into())
            .or_insert_with(|| "8080".into());
        Config::parse_env(&env, std::path::Path::new("/x"), true).unwrap()
    }

    /// The two privileged intents are the ones an operator has to tick in the developer
    /// portal, so they must be on by default (`GUILD_MEMBERS`) or explain themselves
    /// (`MESSAGE_CONTENT`).
    #[test]
    fn intents_are_the_documented_set() {
        let c = cfg(&[("AERONA_TOKEN", TOKEN), ("AERONA_CONTENT_INTENT", "true")]);
        let i = chosen_intents(&c);
        assert!(i.contains(GatewayIntents::GUILD_MEMBERS));
        assert!(i.contains(GatewayIntents::MESSAGE_CONTENT));
        assert!(i.contains(GatewayIntents::GUILD_MODERATION));
        // Voice is never requested: it is 100 % of the surface area of a feature we do
        // not ship, and Discord shows it in the portal checklist.
        assert!(!i.contains(GatewayIntents::GUILD_VOICE_STATES));
        assert!(!i.contains(GatewayIntents::GUILD_PRESENCES));
    }

    #[test]
    fn content_intent_is_opt_in_and_said_out_loud() {
        let c = cfg(&[("AERONA_TOKEN", TOKEN)]);
        assert!(!chosen_intents(&c).contains(GatewayIntents::MESSAGE_CONTENT));
        // `report` is the operator-facing summary; it must not imply the filter is on.
        let text = report(&c);
        assert!(text.contains("local rules only"), "{text}");
        assert!(text.contains("intents:"), "{text}");
        // The names, not `GatewayIntents(512)`: an operator reads this line to know
        // what to tick in the developer portal.
        assert!(text.contains("GUILD_MEMBERS"), "{text}");
        assert!(!text.contains("MESSAGE_CONTENT"), "{text}");
        assert!(
            !text.contains("GatewayIntents("),
            "numeric intent debug is not actionable: {text}"
        );
    }

    /// `connect` refuses an empty token before it touches the network: the failure
    /// must be a configuration error, not a 401 three hundred milliseconds later.
    #[tokio::test]
    async fn a_missing_token_never_reaches_discord() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("AERONA_PORT".into(), "8080".into());
        env.insert("AERONA_BOT".into(), "off".into());
        env.insert(
            "AERONA_DATA_DIR".into(),
            dir.path().join("data").display().to_string(),
        );
        let c = Arc::new(Config::parse_env(&env, dir.path(), true).unwrap());
        let db = aerona_core::db::Db::open(&c).unwrap();
        let state = Arc::new(BotState::new(c, db));
        let err = connect(state, std::future::pending::<()>())
            .await
            .expect_err("no token means no connection");
        assert!(err.to_string().contains("too short"), "{err}");
    }
}
