# Seesaw Adapter Ecosystem - Design Plan

**Status:** ~~Design Phase~~ **CANCELLED - Use Libraries Directly**
**Updated:** 2026-01-29
**Authors:** Design discussion between user and Claude

## TL;DR

**We killed this idea.** Adapters add ceremony without value.

**Instead:** Use standard libraries directly in your effects. See `examples/` for patterns.

## Table of Contents

1. [Vision](#vision)
2. [Core Principle](#core-principle)
3. [Extension Points Taxonomy](#extension-points-taxonomy)
4. [Planned Adapters](#planned-adapters)
5. [Key Patterns](#key-patterns)
6. [Critical Design Decisions](#critical-design-decisions)
7. [Implementation Roadmap](#implementation-roadmap)
8. [Open Questions](#open-questions)

---

## Vision

**The Golden Rule of Extension:**

> You never extend Seesaw by adding power to machines.
> You extend it by adding new ways events enter and new ways commands are executed.

**Machines stay dumb forever.**

### What Makes an Adapter Worth Building?

An adapter is worth building when it provides value beyond a thin wrapper:

❌ **Not worth it:** Just wrapping an SDK with no added value
✅ **Worth it:** Provides event-driven patterns, failure modeling, or conceptual unification

**Example:** `seesaw-anthropic` is valuable because it turns LLM tool use from callback hell into native Commands/Events, not because it wraps the API.

---

## Core Principle

### Mental Map

```
[ External World ]
       ↓
[ Event Adapters ]  ← Turn external signals into events
       ↓
[ Seesaw Machine ]  ← Pure, dumb, deterministic decision function
       ↓
[ Commands ]
       ↓
[ Command Adapters ] ← Execute commands, handle IO/retry/failure
       ↓
[ Events ]
```

**Everything interesting happens around the machine, not inside it.**

---

## Extension Points Taxonomy

Ranked from most common to most specialized:

### 1. Command Adapters (Execution Layer)

**What:** Take commands and attempt to make them real
**Pattern:** `Command → Adapter → Events`

Examples:
- HTTP client adapters
- Database writers
- Background job dispatchers
- AI/LLM calls
- Scrapers, crawlers
- Message queue publishers

**Rules:**
- ✅ May do IO
- ✅ May retry
- ✅ May fail
- ✅ Must emit events describing outcomes
- ❌ Must never emit commands (exception: Reflexive Effects, see below)

**This is where:**
- Retries live
- Rate limiting lives
- Backoff lives
- Circuit breakers live

---

### 2. Event Adapters (Ingress Layer)

**What:** Turn external signals into events
**Pattern:** `External Signal → Adapter → Event`

Examples:
- HTTP endpoints
- Webhooks
- Cron jobs
- Queue consumers
- Admin actions
- Feature flags
- User interfaces

**Rules:**
- ✅ Validate inputs
- ✅ Enrich with IDs/metadata
- ❌ Never decide behavior
- ❌ Never emit commands directly

**Event adapters answer:** "What just happened?"

---

### 3. Failure & Compensation Adapters

**What:** Turn execution failures into structured events

Examples:
- `ScrapeFailed`
- `AuthorizationDenied`
- `RateLimited`
- `ExternalServiceUnavailable`

**Why this matters:**

Without these:
- ❌ Machines deadlock
- ❌ Workflows silently stall
- ❌ State becomes poisoned

With them:
- ✅ Machines can recover
- ✅ Humans can intervene
- ✅ Retries can be modeled

---

### 4. Schedulers & Time Adapters

**Because machines can't see time.**

Examples:
- Cron → `DailyTick`
- Job timeout → `JobTimedOut`
- SLA breach → `EscalationTriggered`

Time becomes just another event.

This unlocks:
- Time-based retries
- Expirations
- Escalations
- Deferred workflows

---

### 5. Observability Integrations

**Seesaw is perfect for observability.**

What to integrate:
- Command issuance
- Event causality
- Workflow duration
- Failure rates
- Deduplication hits

Because:
- Every decision is explicit
- Every transition is event-backed

You get:
- Audit trails
- Replay debugging
- Causal graphs "for free"

---

### 6. AI/LLM Integrations (High leverage, safe)

**AI belongs only in adapters.**

Pattern: `Command → AI Adapter → Success/Failure Event`

Machines:
- ❌ Never see prompts
- ❌ Never see raw outputs
- ✅ Only see structured results

This gives you:
- Determinism
- Retry safety
- Model swap freedom

**Special Case: Tool Use (see Reflexive Effects pattern below)**

---

## Planned Adapters

### Phase 1: Foundation (Establish Patterns)

#### 1. `seesaw-http` ⭐ Reference Implementation

**Purpose:** Establish the command adapter pattern with a simple, well-understood use case

**API:**
```rust
pub struct HttpClient {
    inner: reqwest::Client,
    rate_limiter: Option<RateLimiter>,
}

impl HttpClient {
    pub fn new() -> Self;
    pub fn with_rate_limit(self, requests_per_second: u32) -> Self;
    pub async fn fetch(&self, url: &Url) -> Result<FetchResponse>;
}

pub struct FetchResponse {
    pub url: Url,
    pub status: u16,
    pub content: String,
    pub content_type: String,
}
```

**Features:**
- Rate limiting built-in
- Structured error responses
- Timeout handling
- Retry with backoff

**Example Usage:**
```rust
struct FetchEffect;

#[async_trait]
impl Effect<MyCommand, Deps> for FetchEffect {
    type Event = MyEvent;

    async fn execute(&self, cmd: MyCommand, ctx: EffectContext<Deps>)
        -> Result<MyEvent>
    {
        let MyCommand::FetchUrl { url } = cmd;

        match ctx.deps().http.fetch(&url).await {
            Ok(response) => Ok(MyEvent::Fetched { content: response.content }),
            Err(e) => Ok(MyEvent::FetchFailed { reason: e.to_string() }),
        }
    }
}
```

---

#### 2. `seesaw-anthropic` ⭐ Killer Feature

**Purpose:** Make AI tool use native to Seesaw's event model

**Why This Is Compelling:**

AI tool use is already an event/command system — SDKs just hide it badly.

What you get that raw SDKs cannot give you:
1. **Tool use becomes first-class intent** - Typed, serializable, auditable
2. **AI is no longer "special"** - Just another decision participant
3. **Real audit trail of reasoning** - What tools were used, in what order, with what inputs
4. **Replay becomes meaningful** - Replay tool calls and results without re-running the LLM

**Core Concept:**

> **"Tools are just Commands"**

This is not an integration — it's a conceptual unification.

**API:**
```rust
// Tool registry converts Anthropic tool calls → your Commands
pub struct ToolRegistry<C: Command> {
    tools: HashMap<String, ToolDef<C>>,
}

impl<C: Command> ToolRegistry<C> {
    pub fn register(
        name: &str,
        definition: ToolDefinition,
        parser: impl Fn(Value, String) -> C,
    ) -> Self;

    pub fn to_anthropic_tools(&self) -> Vec<AnthropicTool>;
    pub fn parse_tool_call(&self, name: &str, input: &Value, id: &str) -> Result<C>;
}

// Anthropic API client
pub struct AnthropicClient {
    api_key: String,
    http: reqwest::Client,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self;
    pub async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;
}
```

**Example Usage:**
```rust
// 1. Define your domain tools as Commands
#[derive(Debug, Clone, Serialize, Deserialize)]
enum ResearchCommand {
    StartConversation { conversation_id: Uuid, message: String },
    SearchWeb { tool_call_id: String, query: String },
    FetchUrl { tool_call_id: String, url: String },
}

// 2. Register tools
let tools = ToolRegistry::new()
    .register("search_web", ToolDefinition { /* ... */ }, |params, id| {
        ResearchCommand::SearchWeb {
            tool_call_id: id,
            query: params["query"].as_str().unwrap().to_string(),
        }
    });

// 3. Use in a Reflexive Effect (see pattern below)
```

**See:** [Reflexive Effects Pattern](#reflexive-effects-pattern) for full implementation

---

### Phase 2: Common Infrastructure

#### 3. `seesaw-scheduler`

**Purpose:** Time-based event emission (cron, delays, timeouts)

**API:**
```rust
pub struct Scheduler {
    // TBD
}

impl Scheduler {
    pub fn schedule_once(delay: Duration, event: impl Event);
    pub fn schedule_recurring(cron: CronSpec, event: impl Event);
    pub fn timeout(duration: Duration, event: impl Event) -> TimeoutHandle;
}
```

---

#### 4. `seesaw-postgres` (Adapter utilities)

**Note:** Not a full ORM, just adapter patterns for common Postgres operations

**Purpose:**
- Command → SQL → Event patterns
- Transactional outbox helpers
- Event store helpers

---

### Phase 3: Specialized Adapters (As Needed)

- `seesaw-redis` - Cache/pub-sub adapter
- `seesaw-webhook` - Generic webhook receiver → events
- `seesaw-slack` - Slack notifications & button callbacks → events
- `seesaw-email` - Email send/receive as events
- `seesaw-scraper` - Web scraping with rate limiting

---

## Key Patterns

### Reflexive Effects Pattern

**Problem:** Some external systems (LLMs, human approvers) make decisions and emit commands. How do we model this without violating "machines decide; effects execute"?

**Solution:** Define a special effect type that can emit commands, with strict rules.

#### Rules for Reflexive Effects

1. **Only use for external decision systems** (AI, human approval, external orchestrators)
2. **Commands emitted must be domain commands**, never system commands
3. **The effect must return an event documenting what commands were emitted**
4. **Machine remains the authority** - it decides whether to continue the loop

This pattern is safe because:
- The external system's "decisions" are recorded as events
- The machine can veto or redirect based on those events
- Full audit trail is preserved

#### API

```rust
/// A special effect type that can emit commands in response to external
/// decision-making systems (like LLMs, human approval workflows, etc.)
#[async_trait]
pub trait ReflexiveEffect<C: Command, D>: Send + Sync {
    type Event: Event;

    async fn execute(
        &self,
        cmd: C,
        ctx: ReflexiveContext<D>,
    ) -> Result<Self::Event>;
}

/// Context for reflexive effects - can emit commands
pub struct ReflexiveContext<D> {
    deps: Arc<D>,
    command_emitter: CommandEmitter,
}

impl<D> ReflexiveContext<D> {
    pub fn deps(&self) -> &D;

    /// Emit a command. This will be dispatched after the effect completes.
    pub fn emit_command(&self, cmd: impl Command);
}
```

#### Example: AI Tool Use

```rust
struct ConversationEffect {
    client: AnthropicClient,
}

#[async_trait]
impl ReflexiveEffect<ResearchCommand, Deps> for ConversationEffect {
    type Event = ResearchEvent;

    async fn execute(
        &self,
        cmd: ResearchCommand,
        ctx: ReflexiveContext<Deps>,
    ) -> Result<ResearchEvent> {
        let conversation_id = /* extract from cmd */;

        // Load conversation from DB (NOT from machine state!)
        let conversation = ctx.deps().db.load_conversation(conversation_id).await?;

        // Call Anthropic with tools
        let response = self.client.create_message(CreateMessageRequest {
            model: "claude-sonnet-4-20250514",
            messages: conversation.to_messages(),
            tools: ctx.deps().tools.to_anthropic_tools(),
            max_tokens: 4096,
        }).await?;

        match response.stop_reason {
            StopReason::ToolUse => {
                // Parse tool calls and emit as commands
                let tool_uses = response.content.tool_uses();

                for tool_use in &tool_uses {
                    let command = ctx.deps()
                        .tools
                        .parse_tool_call(&tool_use.name, &tool_use.input, &tool_use.id)?;

                    ctx.emit_command(command); // Execute after effect completes
                }

                // Persist conversation state
                ctx.deps().db.save_conversation(&conversation).await?;

                // Return event documenting what happened
                Ok(ResearchEvent::AgentStep {
                    conversation_id,
                    tool_calls_emitted: tool_uses.len(),
                    is_complete: false,
                })
            }

            StopReason::EndTurn => {
                Ok(ResearchEvent::ResearchComplete {
                    conversation_id,
                    answer: response.content.text(),
                })
            }

            _ => bail!("unexpected stop reason"),
        }
    }
}
```

#### The Machine Stays Minimal

```rust
struct ResearchMachine; // No state!

impl Machine for ResearchMachine {
    type Event = ResearchEvent;
    type Command = ResearchCommand;

    fn decide(&mut self, event: &ResearchEvent) -> Option<ResearchCommand> {
        match event {
            ResearchEvent::QuestionAsked { conversation_id, question } => {
                Some(ResearchCommand::StartConversation {
                    conversation_id: *conversation_id,
                    initial_message: question.clone(),
                })
            }

            ResearchEvent::AgentStep {
                conversation_id,
                tool_calls_emitted,
                is_complete,
                ..
            } => {
                if *is_complete {
                    None // Done
                } else if *tool_calls_emitted > 0 {
                    None // Wait for tool results
                } else {
                    Some(ResearchCommand::ContinueConversation {
                        conversation_id: *conversation_id,
                    })
                }
            }

            ResearchEvent::ToolCompleted { conversation_id, .. } => {
                Some(ResearchCommand::ContinueConversation {
                    conversation_id: *conversation_id,
                })
            }

            _ => None,
        }
    }
}
```

**Key Points:**
- Machine has zero state
- Conversation lives in DB
- Machine just reacts to events
- Clean, testable, replayable

---

### Multi-Machine Coordination Pattern

**Problem:** How do multiple machines coordinate without nesting?

**Solution:** Event coupling, not parent/child calls.

#### Example: Research with Multiple Fetch Machines

```
QuerySubmitted
  → ResearchQueryMachine emits PlanResearch

ResearchPlanCreated { sources: Vec<Url> }
  → FetchFanoutTap emits N × SourceFetchRequested

SourceFetchRequested
  → FetchMachine(source_id) emits FetchSource

SourceFetched / SourceFailed
  → PersistSourceTap increments counter
  → Emits QueryEvent::SourceCompleted

QueryEvent::SourceCompleted (Nth time)
  → ResearchQueryMachine emits GenerateSummary
```

**Key Principles:**
1. **Machines don't coordinate each other** - Events do
2. **EventTaps handle fan-out** - Not machines
3. **State lives in DB** - Not in machine
4. **Machines track thresholds** - Not full data

---

## Critical Design Decisions

### Decision 1: When to Build an Adapter vs Use Raw Deps

**Build an adapter when:**
- ✅ It provides event-driven patterns the raw SDK can't
- ✅ It handles failure/retry as events automatically
- ✅ It enables conceptual unification (like "tools are commands")
- ✅ It provides common patterns that integrate with Seesaw's model

**Use raw deps when:**
- ❌ It's just a thin wrapper with no added value
- ❌ The operation is simple and one-off
- ❌ Event modeling doesn't add clarity

**Example:**
- `seesaw-anthropic` ✅ - Tool use becomes event-driven
- `seesaw-uuid-generator` ❌ - Just use `uuid::Uuid::new_v4()` directly

---

### Decision 2: Reflexive Effects Are Opt-In

**Not all effects can emit commands.**

```rust
trait Effect           // Normal effects (99% of cases)
trait ReflexiveEffect  // Special, opt-in, documented
```

This prevents accidental violations of "machines decide; effects execute."

---

### Decision 3: Machine State Must Be Discardable

**The smell test:**

> If losing machine state would be scary, it doesn't belong there.

**Machines should hold:**
- ✅ Coordination metadata (counters, thresholds, flags)
- ✅ Deduplication keys
- ✅ Workflow stage indicators

**Machines should NOT hold:**
- ❌ Full content blobs
- ❌ Aggregated summaries
- ❌ User-facing data
- ❌ Anything that would be painful to lose

**Where does that data live?**
- In the database
- Effects read it when needed

---

### Decision 4: Replay Means Tool Replay, Not LLM Replay

**Be precise about what "replay" means:**

✅ **Replayable:**
- Tool calls
- Tool results
- Conversation steps
- Command/event flow

❌ **Not replayable:**
- LLM token generation (stochastic)

**But:** You CAN replay with cached LLM responses, which is still incredibly valuable for debugging and testing.

---

## Implementation Roadmap

### Phase 1: Core Infrastructure (Weeks 1-2)

**Goal:** Establish patterns and APIs

1. **Define core adapter traits**
   - `Effect` (already exists)
   - `ReflexiveEffect` (new)
   - `EventTap` (already exists)

2. **Create `seesaw-http`**
   - Reference implementation
   - Validates adapter pattern
   - Documentation and examples

3. **Create `seesaw-anthropic` (MVP)**
   - `AnthropicClient` wrapper
   - `ToolRegistry` implementation
   - `ReflexiveEffect` for conversations
   - Example: Simple Q&A bot with one tool

**Deliverables:**
- ✅ `crates/seesaw-http/`
- ✅ `crates/seesaw-anthropic/`
- ✅ `examples/http-scraper/`
- ✅ `examples/ai-research-assistant/`
- ✅ Pattern documentation

---

### Phase 2: Polish & Real-World Testing (Weeks 3-4)

**Goal:** Validate patterns with real use cases

1. **Expand `seesaw-anthropic`**
   - Multi-turn conversations
   - Streaming support (events as chunks)
   - Error handling and retry
   - Cost tracking

2. **Build reference application**
   - Research assistant that uses both adapters
   - Demonstrates multi-machine coordination
   - Shows reflexive effects in action
   - Full test coverage

3. **Documentation**
   - Adapter design guide
   - When to build an adapter
   - Reflexive effects pattern guide
   - Migration guide from raw SDKs

**Deliverables:**
- ✅ Production-ready `seesaw-anthropic`
- ✅ Reference application
- ✅ Comprehensive documentation

---

### Phase 3: Ecosystem Growth (Ongoing)

**Goal:** Add adapters as needed

- `seesaw-scheduler` - When time-based workflows are needed
- `seesaw-postgres` - When transactional patterns emerge
- `seesaw-redis` - For pub/sub use cases
- `seesaw-webhook` - For external integrations

**Strategy:** Build adapters in response to real needs, not speculatively.

---

## Open Questions

### Q1: How to handle streaming responses?

**Context:** Anthropic supports streaming, where tokens arrive incrementally.

**Options:**
1. **Event per chunk** - `ChunkReceived { text: String }`
2. **Single event with iterator** - `StreamStarted { stream: Stream }`
3. **Buffered completion** - Wait for full response, no streaming

**Leaning towards:** Option 3 for MVP (simplicity), Option 1 for Phase 2 (reactivity)

**Needs discussion:** How do machines react to streaming? Do we need a special pattern?

---

### Q2: How to handle conversation context limits?

**Context:** LLMs have token limits. Long conversations need truncation.

**Options:**
1. **Effect responsibility** - Effect truncates before calling API
2. **Separate command** - `TruncateConversation` command
3. **Automatic** - DB stores truncation logic

**Leaning towards:** Option 1 (effect responsibility)

**Needs discussion:** What's the right boundary?

---

### Q3: How to handle rate limiting?

**Context:** APIs have rate limits. Adapters need to handle them.

**Options:**
1. **Adapter-level** - `HttpClient` has built-in rate limiter
2. **Effect-level** - Each effect manages its own rate limiting
3. **Command-level** - Commands specify rate limit groups

**Leaning towards:** Option 1 (adapter-level)

**Implemented in:** `seesaw-http` as reference

---

### Q4: Should we support OpenAI in addition to Anthropic?

**Context:** OpenAI also has tool calling, similar patterns.

**Options:**
1. **Separate crate** - `seesaw-openai` with similar API
2. **Unified crate** - `seesaw-llm` with providers
3. **Wait for demand** - Start with Anthropic, add OpenAI if needed

**Leaning towards:** Option 3 (wait for demand)

**Reasoning:** Validate patterns with one provider first, then generalize.

---

### Q5: How to version tool definitions?

**Context:** As tool definitions evolve, old conversations may break.

**Options:**
1. **Versioned tools** - `search_web_v1`, `search_web_v2`
2. **Schema evolution** - Tools handle old/new formats
3. **Conversation snapshots** - Store tool definitions with conversation

**Leaning towards:** Option 2 (schema evolution)

**Needs discussion:** How does this interact with replay?

---

## Success Criteria

### For `seesaw-http`

✅ **MVP Success:**
- Clean API with rate limiting
- Structured error responses
- Example application demonstrating usage
- Documentation

✅ **Long-term Success:**
- Adopted as the standard way to do HTTP in Seesaw apps
- Other adapters follow the same pattern
- Community contributions and extensions

---

### For `seesaw-anthropic`

✅ **MVP Success:**
- Tool use works end-to-end
- Reflexive effects pattern validated
- Example AI agent with 2+ tools
- Clear documentation of when to use this

✅ **Long-term Success:**
- Developers prefer this over raw Anthropic SDK
- Complex AI agents are built on this foundation
- Pattern extends to other LLM providers
- Becomes a flagship Seesaw feature

---

## Next Steps

1. **Finalize adapter APIs** - Review and iterate on trait definitions
2. **Implement `seesaw-http`** - Start with reference implementation
3. **Implement `seesaw-anthropic` MVP** - Tool use only, no streaming
4. **Build example applications** - Validate patterns with real code
5. **Write comprehensive documentation** - Pattern guides and tutorials

---

## Notes & Considerations

### On Machine Boundaries

> "If losing machine state would be scary, it doesn't belong there."

This is the core smell test. Use it religiously.

### On Reflexive Effects

This pattern is powerful but dangerous if misused. Document extensively:
- When to use it
- When NOT to use it
- How it differs from normal effects
- What the rules are

### On Adapter Value

Not every integration needs an adapter. Build adapters when they provide:
1. Event-driven patterns
2. Conceptual unification
3. Reusable failure handling
4. Testing/mocking boundaries

Otherwise, just use deps directly.

### On Replay

Be precise about what "replay" means. It's not magic determinism - it's structured event replay with cached external responses.

---

## References

- [CLAUDE.md](./CLAUDE.md) - Core Seesaw principles
- [README.md](./README.md) - Project overview
- Extension points guide (in CLAUDE.md)
- Reflexive effects pattern (this document)

---

## Changelog

- **2026-01-29** - Initial design document created
- Design discussions captured from conversation context
- Reflexive effects pattern defined
- Adapter taxonomy established
