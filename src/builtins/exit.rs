//! Exit: terminates only the calling agent. The tool reaches the calling
//! agent's per-agent child cancellation token through the [`ToolCallContext`]
//! and cancels it, ending that agent's run loop while siblings and the harness
//! keep running. Its constructor takes no behavioral arguments; the self-stop
//! handle arrives at call time through the context.

use serde_json::{Value, json};

use crate::tool::{Tool, ToolCallContext, ToolError};

pub struct Exit {
    schema: Value,
}

impl Exit {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }
}

impl Default for Exit {
    fn default() -> Self {
        Self::new()
    }
}

#[jsonrpsee::core::async_trait]
impl Tool for Exit {
    fn name(&self) -> &str {
        "exit"
    }

    fn description(&self) -> &str {
        "Terminate this agent, releasing its task so it no longer processes \
         inbound messages. Only this agent stops; sibling agents and the harness \
         keep running. Takes no parameters. Returns `{status: \"exiting\"}`."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    async fn call(&self, _input: Value) -> Result<Value, ToolError> {
        Err(ToolError::InvalidInput(
            "exit requires the calling agent's context and cannot be called \
             without it"
                .to_string(),
        ))
    }

    async fn call_with_context(
        &self,
        _input: Value,
        ctx: &ToolCallContext,
    ) -> Result<Value, ToolError> {
        ctx.stop();
        Ok(json!({ "status": "exiting" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use crate::agent::{Models, SystemPrompt};
    use crate::harness::{
        AgentSpawner, AgentSpec, Error as HarnessError, HarnessBuilder, ModelId, PendingAgent,
    };
    use crate::history::AgentId;
    use crate::messagebus::AgentEvent;
    use crate::model::{ARModel, ModelRole, ModelStreamEvent, Usage};
    use crate::test_support::StubArModel;
    use crate::tool::ToolRegistry;

    /// Minimal [`AgentSpawner`] the unit test can hand to a bare
    /// [`ToolCallContext`]; Exit never spawns, so it only needs to satisfy the
    /// type.
    struct NoopSpawner;

    impl AgentSpawner for NoopSpawner {
        fn spawn(&self, _spec: AgentSpec) -> Result<AgentId, HarnessError> {
            Ok(AgentId::new())
        }
    }

    fn prompt(base: &str) -> SystemPrompt {
        SystemPrompt {
            base: base.to_string(),
            skills: None,
        }
    }

    fn models() -> HashMap<ModelRole, Arc<dyn ARModel>> {
        let mut map: HashMap<ModelRole, Arc<dyn ARModel>> = HashMap::new();
        map.insert(ModelRole::Default, Arc::new(StubArModel::empty()));
        map
    }

    #[tokio::test]
    async fn call_with_context_cancels_self_stop_and_reports_exiting()
    -> Result<(), Box<dyn std::error::Error>> {
        let token = CancellationToken::new();
        let ctx = ToolCallContext::new(
            AgentId::new(),
            None,
            prompt("agent"),
            Models::new(models())?,
            Arc::new(ToolRegistry::new()),
            Arc::new(NoopSpawner),
            token.clone(),
        );
        let tool = Exit::new();

        assert!(!token.is_cancelled(), "token starts live");
        let value = tool
            .call_with_context(json!({}), &ctx)
            .await
            .expect("exit succeeds");

        assert_eq!(value["status"], "exiting");
        assert!(
            token.is_cancelled(),
            "exit must cancel the calling agent's self-stop token",
        );
        Ok(())
    }

    #[tokio::test]
    async fn call_without_context_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let tool = Exit::new();
        let err = tool
            .call(json!({}))
            .await
            .expect_err("exit cannot run without the calling context");
        assert!(matches!(err, ToolError::InvalidInput(_)));
        Ok(())
    }

    #[tokio::test]
    async fn agent_holding_exit_terminates_after_calling_it()
    -> Result<(), Box<dyn std::error::Error>> {
        // Call 1 emits a tool use for exit; call 2 (the re-entry after the tool
        // result) yields nothing, so the inner loop breaks and the agent returns
        // to awaiting inbound, where it observes its self-stop.
        let model = Arc::new(StubArModel::with_call_scripts(vec![
            vec![
                Ok(ModelStreamEvent::ToolUseComplete {
                    id: "use_1".to_string(),
                    name: "exit".to_string(),
                    input: json!({}),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage::default(),
                }),
            ],
            vec![Ok(ModelStreamEvent::MessageComplete {
                usage: Usage::default(),
            })],
        ]));

        let model_id = ModelId::new("stub");
        let agent_id = AgentId::new();
        let mut harness = HarnessBuilder::new()
            .add_model(model_id.clone(), model)
            .add_tool(Arc::new(Exit::new()))
            .expect("add exit tool")
            .add_agent(PendingAgent {
                id: agent_id,
                system_prompt: prompt("test"),
                model_id,
            })
            .build()
            .expect("build harness");
        harness.start().expect("start");

        let mut sub = harness.subscribe(agent_id).expect("subscribe");
        let owner = harness.owner_id();
        harness
            .send_to_agent(agent_id, owner, "please exit".to_string())
            .expect("send seed message");

        // The tool runs while draining to Idle: it self-stops.
        timeout(Duration::from_secs(2), async {
            while let Some(evt) = sub.next().await {
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        })
        .await
        .expect("idle wait timed out");

        // The self-stop dropped the agent's inbound consumer, so a post-exit send
        // is refused: the agent no longer processes inbound.
        let post_exit = harness.send_to_agent(agent_id, owner, "ignored".to_string());
        assert!(
            post_exit.is_err(),
            "exited agent must no longer accept inbound, got {post_exit:?}"
        );

        // join WITHOUT shutdown confirms termination: the agent's inbound is
        // never closed and shutdown is never called, so join can only return
        // because the self-stop already ended the task; a live agent would hang.
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out — agent did not exit")
            .expect("clean exit");
        Ok(())
    }

    #[tokio::test]
    async fn sibling_is_unaffected_when_one_agent_exits() -> Result<(), Box<dyn std::error::Error>>
    {
        // Agent A calls exit and terminates. Agent B answers a message after A is
        // gone, proving the exit is scoped to A and join stays clean.
        let model_a = Arc::new(StubArModel::with_call_scripts(vec![
            vec![
                Ok(ModelStreamEvent::ToolUseComplete {
                    id: "use_1".to_string(),
                    name: "exit".to_string(),
                    input: json!({}),
                }),
                Ok(ModelStreamEvent::MessageComplete {
                    usage: Usage::default(),
                }),
            ],
            vec![Ok(ModelStreamEvent::MessageComplete {
                usage: Usage::default(),
            })],
        ]));
        let model_b = Arc::new(StubArModel::with_events(vec![
            Ok(ModelStreamEvent::ContentComplete {
                text: "alive".to_string(),
            }),
            Ok(ModelStreamEvent::MessageComplete {
                usage: Usage::default(),
            }),
        ]));

        let model_a_id = ModelId::new("stub-a");
        let model_b_id = ModelId::new("stub-b");
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();

        let mut harness = HarnessBuilder::new()
            .add_model(model_a_id.clone(), model_a)
            .add_model(model_b_id.clone(), model_b)
            .add_tool(Arc::new(Exit::new()))
            .expect("add exit tool")
            .add_agent(PendingAgent {
                id: agent_a,
                system_prompt: prompt("a"),
                model_id: model_a_id,
            })
            .add_agent(PendingAgent {
                id: agent_b,
                system_prompt: prompt("b"),
                model_id: model_b_id,
            })
            .build()
            .expect("build harness");
        harness.start().expect("start");

        // Drive agent A to exit.
        let mut sub_a = harness.subscribe(agent_a).expect("subscribe a");
        let owner = harness.owner_id();
        harness
            .send_to_agent(agent_a, owner, "please exit".to_string())
            .expect("send a");
        timeout(Duration::from_secs(2), async {
            while let Some(evt) = sub_a.next().await {
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        })
        .await
        .expect("agent a idle wait timed out");

        let post_exit = harness.send_to_agent(agent_a, owner, "ignored".to_string());
        assert!(post_exit.is_err(), "exited agent a must refuse inbound");

        // Agent B still runs: it processes a message and reaches Idle.
        let mut sub_b = harness.subscribe(agent_b).expect("subscribe b");
        harness
            .send_to_agent(agent_b, owner, "ping".to_string())
            .expect("send b");
        let mut saw_alive = false;
        timeout(Duration::from_secs(2), async {
            while let Some(evt) = sub_b.next().await {
                if let AgentEvent::BlockComplete { block } = &evt
                    && let crate::history::Block::AgentMessage { content, .. } = block.block()
                    && content == "alive"
                {
                    saw_alive = true;
                }
                if matches!(evt, AgentEvent::Idle) {
                    break;
                }
            }
        })
        .await
        .expect("agent b idle wait timed out");
        assert!(saw_alive, "sibling agent B should keep running and answer");

        // Close B's inbound so its loop exits. A's inbound is never closed and
        // shutdown is never called, so join can only return because A exited; a
        // live A would hang join.
        harness.bus().close_inbound(agent_b);
        timeout(Duration::from_secs(5), harness.join())
            .await
            .expect("join timed out — exited agent kept the harness alive")
            .expect("clean join");
        Ok(())
    }
}
