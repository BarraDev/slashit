# Extensibility direction

Status: future design direction only. Nothing here is approved or implemented,
and it commits SlashIt to no plugin runtime, SDK, ABI, packaging format or
timeline.

## Direction

SlashIt may eventually be extended without changing its core. The order of
concerns, if this is pursued:

1. **Internal extension contracts first.** Give SlashIt's own capabilities
   narrow, explicit, versionable boundaries (for example the bounded context
   and lookup surface described in
   [project-conversation-history.md](project-conversation-history.md), and the
   forge boundary for GitHub operations). A contract is proven by internal
   implementations before anything is exposed.
2. **Local third-party plugins later**, running on the user's machine against
   those proven contracts.
3. **Later concerns, taken up only after concrete needs emerge:**
   distribution, permissions and sandboxing, versioning and compatibility,
   updates, and any marketplace.

## Possible extension points

Candidates only, none of them a requirement: context sources, memory, tools,
integrations, and UI.

## Constraints to keep

- An extension never gains authority SlashIt would not grant a built-in: Task
  execution stays Human-gated, and the authoritative Conversation and approval
  records stay in their domain records.
- Extension output is evidence for the model, not instruction.
- Do not choose a runtime, SDK, ABI or packaging model before at least one
  internal contract has more than one real implementation.
