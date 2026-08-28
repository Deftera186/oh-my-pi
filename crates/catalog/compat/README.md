# Compatibility taxonomy and cascade

This tree is the checked-in source of model identity and compatibility policy. A **class** is a vendor lineage such as `gemini` or `anthropic`; a **family** is a product line within one class, such as `flash`, `pro`, `sonnet`, or `opus`; and a **revision** is an `omp_core::SemVer` value (`major.minor.patch`) extracted from a model name. Missing minor and patch components compare as zero.

There are three ownership strata:

- `taxonomy/*.kdl` defines identity: class membership, product families, revision extraction, reviewed exact corrections, and suffix collapse.
- `classes/*.kdl` defines model-lineage truths: behavior inherent to a model line, optionally scoped to the providers where the census established it.
- `providers/*.kdl` defines deployment contracts: behavior imposed by a host, plus documented per-model residue that taxonomy cannot express exactly.

Do not move a statistically common provider behavior into a class file, or a lineage truth into a provider file. Absence is not evidence that a capability is stripped. Preserve comments that record census provenance, reviewed exceptions, and why a `models` residue remains. Source-lock entries use the provenance text `census 2026-08: .omp/local/quirks + frozen oracle`.

Both grammars are KDL v2. Unknown nodes/directives and malformed value shapes are errors. Declaration and file order do not break cascade ties.

## Taxonomy grammar

At a taxonomy document root, the only permitted nodes are `class`, `collapse`, and `discovery`; a source may contain multiple class nodes. Class names and override IDs must be unique across all bundled sources. Exactly one non-empty `collapse` definition is required across the inventory; at most one `discovery` definition may appear.

```kdl
class "anthropic" {
    namespace "anthropic" bounded=#true
    bounded "claude"

    family "sonnet" glob="*sonnet*"
    family "opus" glob="*opus*"

    revision prefix="claude-" anywhere=#true

    override id="reviewed-distill" provider="example-host" model="opaque-model" \
        logical="author/opaque-model" class="anthropic" family="opus" revision="4.6" \
        effort="high" thinking-variant=#true expires-at-ms=1799712000000 \
        rationale="Reviewed teacher lineage" provenance="frozen census case identity-01"
}
```

### Class membership matchers

Classification trims and lowercases the full model identifier. The **bare name** is the segment after its final `/`. Matcher tokens are also lowercased while parsing.

| Node | Rank | Match |
| --- | ---: | --- |
| `exact "token"` | 4 | The whole bare name equals `token`. |
| `bounded "token"` | 3 | The bare name equals `token`, or starts with it followed by `-`, `_`, `.`, `:`, or an ASCII digit. |
| `namespace "token"` | 2 | A non-empty `/`-separated segment of the full identifier equals `token`. |
| `namespace "token" bounded=#true` | 2 | Split the full identifier on `/`, `.`, and `:`; a segment must satisfy the bounded rule above. This is the only matcher property. |
| `prefix "token"` | 1 | The bare name starts with `token`. |
| `glob "pattern"` | 0 | An anchored `*` wildcard match over the bare name. `*` spans any substring; all non-wildcard text remains anchored in order. |

A class match is ranked by `(matcher-kind rank, token byte length)`. The greatest tuple wins. Equal tuples from different classes are an `AmbiguousClass` error; source order is not a tiebreak. If nothing matches, classification returns class `unknown` with no family or revision.

### Product families

A family rule has one name, a required `glob` property, and an optional signed integer `priority` (default `0`):

```kdl
family "flash" glob="*flash*"
family "lite" glob="*flash-lite*" priority=10
```

The glob is anchored, ASCII-case-insensitive, and matched against the lowercased bare name. Matching families rank by `(priority, non-wildcard byte count in the glob)`. Equal ranks belonging to different family IDs are an `AmbiguousFamily` error. No match produces no family. Repeating rules for the same family ID is allowed, as in the checked-in `o-series` taxonomy.

### Revision extraction

A class may contain both forms:

```kdl
revision prefix="gemini-"
revision prefix="claude-" anywhere=#true
revision skip-bare "o1" "o3" "o4"
```

- `prefix=` adds a lowercased extraction prefix. Without `anywhere=#true`, it must begin the bare name. With it, the first occurrence may appear anywhere in the bare name.
- Prefixes are tried in declaration order; the first matching prefix is used.
- `skip-bare` takes one or more bare names that intentionally carry no revision and overrides extraction.
- After removing or locating the prefix, extraction starts at the first ASCII digit. It reads at most three unsigned 8-bit numeric components separated by `.` or by `-` followed by a digit. Missing components become zero. Thus `claude-opus-4-6` can produce `4.6.0`.

### Reviewed identity overrides

`override` has properties only and no child block. Required string properties are:

- `id`: stable, globally unique review ID;
- `model`: exact bare model identifier, compared case-insensitively;
- `rationale`: human-readable reason for the correction;
- `provenance`: evidence location.

Optional properties are:

| Property | Shape and meaning |
| --- | --- |
| `provider` | Exact provider key, compared case-insensitively. A matching provider-specific override wins over a provider-agnostic one. |
| `logical` | Corrected logical model identifier. |
| `class` | Corrected class ID; a non-empty string. |
| `family` | Corrected product-family ID; a non-empty string. |
| `revision` | One to three unsigned 8-bit components separated by `.` or `-`. |
| `effort` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. |
| `thinking-variant` | Boolean marker for a separately exposed thinking sibling. |
| `expires-at-ms` | Non-negative Unix time in milliseconds. The override is inactive when the observation time is at or after this value. |

The pair `(provider, model)` must also be unique, including provider-agnostic pairs. When no observation time is supplied, an expiring override remains active.

### Suffix collapse

The single collapse vocabulary has this grammar:

```kdl
collapse {
    thinking-suffix "-thinking"
    effort-suffix "-minimal" tier="minimal"
    effort-suffix "-xhigh" tier="xhigh"
    effort-suffix "-max" tier="max" except-bare-prefix="qwen"
    effort-lane-suffix "-fast" "cursor" bare-prefix="cursor-grok"
    effort-family "google-antigravity" "gemini-3.7-flash" "gemini-3.7-flash-tiered"
    routing-variant-suffix "-wm" "openai-codex" "openai-codex-device"
}
```

`thinking-suffix` accepts one non-empty suffix and no properties. `effort-suffix` additionally requires `tier` with one of the effort values above, and may have `except-bare-prefix`. `routing-variant-suffix` takes one non-empty suffix followed by one or more provider IDs: a wire identifier carrying the suffix on one of those providers is a **routing variant** of its plain identifier — discovery derives base-model metadata (key, limits, pricing, thinking) from the plain bundled SKU while keeping the suffixed wire identifier for requests; routing variants never participate in effort collapse. `effort-lane-suffix` takes one non-empty lane suffix followed by one or more provider IDs, plus an optional `bare-prefix` gate: on a declared provider, an identifier ending in the lane suffix collapses by the effort-suffix vocabulary applied immediately before the lane token, and the collapsed logical identifier keeps the lane suffix (`cursor-grok-4.6-low-fast` → `cursor-grok-4.6-fast` at effort `low`) — one logical model per service-tier lane. The lane wraps effort tiers only; thinking suffixes never lane. `effort-family` takes a provider ID, a canonical logical model ID, and zero or more exact member aliases. Declaring a family enables conservative dynamic effort-sibling grouping for that provider; each alias collapses case-insensitively to the canonical logical ID without assigning an effort, so a provider's unsuffixed discovery alias can join the routed family. Provider/logical pairs and provider/alias pairs are unique. Suffixes are unique case-insensitively across all four directives. Matching is case-insensitive against the end of the full model identifier; the longest matching suffix wins. The exception tests the lowercased bare name prefix.

### Discovery vocabulary

```kdl
discovery {
    recover-canonical-params "gmi-cloud"
    borrow-responses-route "opencode-go" "opencode-zen"
    billing-variant-suffix "-free" "-contributor"
}
```

`recover-canonical-params` takes one or more provider IDs (unique case-insensitively). On a declared provider, runtime discovery recovers intrinsic base-model parameters — display name, context window, output limit, and the interned thinking policy — for a discovered **namespaced** identity (`deepseek-ai/…`) from the bundled canonical reference index built across all providers; the first entry in frozen catalog order wins. A bare identity is eligible only when the same provider also declares it through `responses-route-models`, keeping canonical recovery limited to reviewed exact gateway-first pins. Pricing, wire policy, and effort routing are never borrowed across providers., and bare un-namespaced slugs never match.

`borrow-responses-route` takes one or more provider IDs forming a sibling-gateway group; a provider may belong to at most one group across the inventory. On a declared provider, an **unbundled** discovered id — or its billing-variant base — that is bundled on any group member with an `openai-responses` route materializes on the discovering provider's own responses route instead of the discovery route (the OpenCode gateways ship models before any census bundles them). Only the responses signal is borrowed: anthropic and chat transports genuinely diverge across gateways, and pricing, limits, and thinking stay conservative. Declaring a group also indexes every group member's bundled wire identities so an advertised bundled slug keeps its own card even off the discovery route.

`billing-variant-suffix` takes one or more suffixes (unique case-insensitively, not bare `-`). A wire identifier carrying a declared suffix (`gpt-5.5-pro-free`, `muse-spark-1.2-contributor`) shares a transport with its base id for responses-route hinting; nothing else — pricing in particular — is derived from the base SKU.

## Cascade grammar

A cascade document starts with `class` or `provider`. Every selector adds a conjunct to the current rule. Axis directives may appear directly in any permitted scope, and nested selector blocks may appear alongside them.

```kdl
class "gemini" {
    on "google" "google-vertex" "openrouter" {
        family "flash" {
            revision ">=2.5 <3.8" {
                thinking-efforts "minimal" "low" "medium" "high"
            }
        }
    }
}

provider "openrouter" {
    thinking-format "openrouter"
    class "openai" {
        family "o-series" {
            thinking-efforts "minimal" "low" "medium" "high"
        }
    }
    models "openai/o1:batch" "vendor/*-reasoning" priority=10 {
        thinking-requires-effort #true
    }
}
```

### Selectors and nesting

| Selector | Form | Matching semantics |
| --- | --- | --- |
| `class` | `class "id" { ... }` | Exact class ID. At document root it may contain `on`, `family`, `revision`, and `models`. Under `provider` it may contain `family`, `revision`, and `models`. |
| `provider` | `provider "id" { ... }` | Exact provider ID. It is root-only and may contain `class` and `models`. |
| `on` | `on "provider-a" "provider-b" { ... }` | One or more provider IDs, combined as OR. It is allowed only under a root `class`, and may contain `family`, `revision`, and `models`. |
| `family` | `family "id" { ... }` | Exact classified family ID. It may contain `revision` and `models`. A target with no family does not match. |
| `revision` | `revision ">=2.5 <4" { ... }` | A non-empty, whitespace-separated conjunction of comparisons. It may contain `models`. A target with no revision does not match. |
| `models` | `models "id" "vendor/*" { ... }` | One or more alternatives, combined as OR. It cannot contain another selector. |

Class, provider/`on`, and family selector values are compared exactly and case-sensitively to the structured resolve target.

Revision operators are `>=`, `>`, `<=`, `<`, and `=`. Each operand has one to three dot-separated unsigned 8-bit components; omitted components are zero. All terms must hold.

A `models` string without `*` is an exact, case-sensitive match against the provider-relative model identifier. A string containing `*` is an anchored, ASCII-case-insensitive wildcard match. Prefer taxonomy ranks; retain exact/glob lists only when they isolate the census member set exactly, and keep a `// residue:` comment explaining why ranks do not.

`priority=N` is an optional signed integer property on the block that owns axis assignments. Its default is zero. Use it only to resolve an intentional equal-specificity overlap; do not use it to encode declaration order.

### Axis value shapes

The directive vocabulary is closed. Its three shapes are:

- **Scalar**: exactly one KDL boolean, integer, float, or string argument and no children. `#null` is rejected.
- **Array**: one or more scalar arguments and no children; it resolves to a JSON array.
- **Object**: no arguments and a child block, including an empty block. Child names are emitted verbatim as JSON keys. Each child is either one scalar or another object; arrays are not representable inside an object payload.

#### Wire axes

| KDL directive | Resolved key | Shape |
| --- | --- | --- |
| `allows-synthetic-reasoning-content-for-tool-calls` | `allows_synthetic_reasoning_content_for_tool_calls` | Scalar |
| `disable-adaptive-thinking` | `disable_adaptive_thinking` | Scalar |
| `disable-reasoning-on-tool-choice` | `disable_reasoning_on_tool_choice` | Scalar |
| `escape-builtin-tool-names` | `escape_builtin_tool_names` | Scalar |
| `extra-body` | `extra_body` | Object |
| `filter-reasoning-history` | `filter_reasoning_history` | Scalar |
| `flatten-root-unions` | `flatten_root_unions` | Scalar |
| `include-encrypted-reasoning` | `include_encrypted_reasoning` | Scalar |
| `image-encoding-format` | `image_encoding_format` | Scalar |
| `max-tokens-field` | `max_tokens_field` | Scalar |
| `official-endpoint` | `official_endpoint` | Scalar |
| `omit-reasoning-effort` | `omit_reasoning_effort` | Scalar |
| `reasoning-content-field` | `reasoning_content_field` | Scalar |
| `reasoning-disable-mode` | `reasoning_disable_mode` | Scalar |
| `reasoning-effort-map` | `reasoning_effort_map` | Object |
| `replay-unsigned-thinking` | `replay_unsigned_thinking` | Scalar |
| `requires-assistant-content-for-tool-calls` | `requires_assistant_content_for_tool_calls` | Scalar |
| `requires-reasoning-content-for-all-assistant-turns` | `requires_reasoning_content_for_all_assistant_turns` | Scalar |
| `requires-reasoning-content-for-tool-calls` | `requires_reasoning_content_for_tool_calls` | Scalar |
| `requires-thinking-enabled` | `requires_thinking_enabled` | Scalar |
| `requires-tool-result-id` | `requires_tool_result_id` | Scalar |
| `signing-endpoint` | `signing_endpoint` | Scalar |
| `stream-idle-timeout-ms` | `stream_idle_timeout_ms` | Scalar |
| `thinking-close-max-retries` | `thinking_close_max_retries` | Scalar |
| `supports-developer-role` | `supports_developer_role` | Scalar |
| `supports-eager-tool-input-streaming` | `supports_eager_tool_input_streaming` | Scalar |
| `supports-forced-tool-choice` | `supports_forced_tool_choice` | Scalar |
| `supports-image-detail-original` | `supports_image_detail_original` | Scalar |
| `supports-long-cache-retention` | `supports_long_cache_retention` | Scalar |
| `supports-mid-conversation-system` | `supports_mid_conversation_system` | Scalar |
| `supports-reasoning-effort` | `supports_reasoning_effort` | Scalar |
| `supports-reasoning-summary` | `supports_reasoning_summary` | Scalar |
| `supports-sampling-params` | `supports_sampling_params` | Scalar |
| `supports-store` | `supports_store` | Scalar |
| `supports-tool-choice` | `supports_tool_choice` | Scalar |
| `supports-usage-in-streaming` | `supports_usage_in_streaming` | Scalar |
| `thinking-format` | `thinking_format` | Scalar |
| `when-thinking` | `when_thinking` | Object |

Object example:

```kdl
reasoning-effort-map {
    minimal "low"
    xhigh "high"
}
extra-body {
    reasoning {
        enabled #true
    }
}
```

#### Thinking axes

| KDL directive | Resolved key | Shape |
| --- | --- | --- |
| `thinking-default-level` | `defaultLevel` | Scalar |
| `thinking-effort-budgets` | `effortBudgets` | Object |
| `thinking-efforts` | `efforts` | Array |
| `thinking-mode` | `mode` | Scalar |
| `thinking-requires-effort` | `requiresEffort` | Scalar |
| `thinking-suppress-when-off` | `suppressWhenOff` | Scalar |
| `thinking-supports-display` | `supportsDisplay` | Scalar |

A rule cannot assign the same resolved axis twice in one block.
#### Catalog-data directives

| KDL directive | Resolved key | Shape |
| --- | --- | --- |
| `edit-revision` | `editRevision` | Non-empty string scalar |
| `long-context-cost` | `longContext` | Object |

Catalog-data directives patch compiled model metadata rather than request wire
policy. `edit-revision` selects an existing registered edit-tool contract such
as `sloppy.1`; absence preserves the source model's `editRevision` value.

### Precedence and ambiguity

Rules resolve independently per axis. A matching rule is ranked by:

```text
(model-selector exactness, constrained-dimension count, priority)
```

The tuple is compared lexicographically, greatest first:

- model exactness is `2` when any matching `models` selector is exact, `1` when the best matching selector is a glob, and `0` when the rule has no `models` selector;
- dimension count is the number of present dimensions among class, provider/`on`, family, revision, and models;
- priority is the local block's `priority`, defaulting to `0`.

The highest-ranked matching assignment wins for that axis. Two distinct rules that tie on all three components and assign the same axis are an `AmbiguousOverlap` error even if their values are equal. File and declaration order never resolve the tie; add an explicit priority only after confirming the overlap is intentional.

### Capability gating

Wire axes are considered for every matching target. Thinking axes are considered only when the structured resolve target sets `reasoning`; catalog compilation sets it only for a logical model whose source members carry a structural thinking profile. A target without that flag cannot inherit a thinking profile from a matching class, provider, family, revision, or model rule. Family and revision selectors likewise never match targets missing that rank. An unmatched target resolves to empty maps; the cascade does not infer negative capabilities from absence.

## Deterministic regeneration

Run all commands from the workspace root. Keep inputs and generated output in stable sorted order; never accept a selector because it happens to cover only a sampled provider.

### 1. Review the full classified roster

Use the generated review artifact `target/catalog.normalized.json` (written by
`generate_snapshot`, step 3) as the complete compiled roster. Join its model
identities and classifications with `fixtures/llm-oracle/catalog-policy/compat-profiles.json`
and `thinking-profiles.json`. For each desired member set, test candidates against the entire
roster and accept one only when it selects exactly that set within its class and any `on` provider
scope. Use this deterministic candidate order:

1. family;
2. family plus a closed-open revision range (`>=a <b`);
3. revision range;
4. anchored `*` glob synthesis;
5. exact model IDs as documented residue.

Emit class files before provider residues, sort provider/model alternatives deterministically, preserve census comments, and ensure every on-disk file is listed by `BUNDLED_TAXONOMY` or `BUNDLED_COMPAT`.

### 2. Refresh `data/sources.lock.json`

The ID scheme is `compat.cascade.<group>.<stem>.v1`, where group is `taxonomy`, `classes`, `providers`, or `runtime`; paths are workspace-relative. Build the generator while the current lock and snapshots still agree, then refresh the lock. This avoids the intentional build-time check rejecting an old snapshot after its source digest changes.

The following runnable update preserves non-compat IDs and provenance, replaces every compat KDL entry, refreshes every locked hash, sorts by ID, and recomputes `source_digest` as `sha256(concat(id + NUL + path + NUL + sha256 + NUL))`:

```sh
cargo build -p omp-catalog --example generate_snapshot
python3 - <<'PY'
from pathlib import Path
import hashlib, json

root = Path.cwd()
lock_path = root / "crates/catalog/data/sources.lock.json"
lock = json.loads(lock_path.read_text())
inputs = {
    item["id"]: item
    for item in lock["inputs"]
    if not item["id"].startswith("compat.cascade.")
}
for path in sorted((root / "crates/catalog/compat").glob("*/*.kdl")):
    relative = path.relative_to(root).as_posix()
    group, stem = path.parent.name, path.stem
    item_id = f"compat.cascade.{group}.{stem}.v1"
    inputs[item_id] = {
        "id": item_id,
        "path": relative,
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "source": "census 2026-08: .omp/local/quirks + frozen oracle",
    }
for item in inputs.values():
    item["sha256"] = hashlib.sha256((root / item["path"]).read_bytes()).hexdigest()
lock["inputs"] = sorted(inputs.values(), key=lambda item: item["id"])
h = hashlib.sha256()
for item in lock["inputs"]:
    for field in ("id", "path", "sha256"):
        h.update(item[field].encode())
        h.update(b"\0")
lock["source_digest"] = h.hexdigest()
lock_path.write_text(json.dumps(lock, indent=2) + "\n")
PY
```

### 3. Generate the compiled snapshot

Run the generator binary built before the source-lock update:

```sh
./target/debug/examples/generate_snapshot
```

This verifies the source lock and rewrites:

- `crates/catalog/data/catalog.postcard` — the checked-in snapshot embedded at build time;
- `target/catalog.normalized.json` — the full compiled catalog (providers, routes, models,
  wire policies, thinking policies, revision) as a reviewable JSON artifact. Its SHA-256 rides
  the postcard header, so the JSON is reproducible from the checked-in snapshot and stays out
  of git.

If this rewrites the postcard, repeat step 2 and rerun the prebuilt generator. Then verify the
compiled catalog:

```sh
cargo nextest run -p omp-catalog --lib taxonomy
cargo nextest run -p omp-catalog --test compat_cascade
```
