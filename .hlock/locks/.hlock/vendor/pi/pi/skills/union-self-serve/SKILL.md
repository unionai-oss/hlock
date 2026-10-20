---
name: union-self-serve
description: 'Entry point for Union.ai self-serve setup on AWS. Routes to the right step — subscribe and sign up, run a tracked workflow locally, provision AWS resources (EKS/S3/ECR/IAM), connect your cluster, or debug a cluster that will not come up — and defines the state file, approval gates, and UI-handoff protocol the other union skills share. Trigger words: "set up Union", "Union self-serve", "deploy Union.ai", "Union on AWS", "union.ai onboarding", "hosted.unionai.cloud".'
---

# Union.ai self-serve setup

Self-serve setup gives you a **self-managed deployment**: Union.ai runs the control plane,
the data plane runs in *your* EKS cluster, and you own the cluster and its upgrades. You
subscribe on AWS Marketplace, create an organization in the Union.ai UI, and connect a
cluster — Union.ai then installs the data plane into it for you.

**Nothing dials in.** You install an agent into your cluster and it connects *outwards* to
Union.ai. No port is opened, no endpoint exposed, no cluster credential handed over.

Docs: <https://www.union.ai/docs/v2/union/deployment/self-serve/>

> Self-serve setup is AWS-only today. GCP and Azure are "coming soon" — if the user is on
> either, say so and point them at the manual
> [self-managed guide](https://www.union.ai/docs/v2/union/deployment/selfmanaged.md)
> instead of improvising.

---

## The five steps, and who does what

| # | Step | Who drives | Skill |
|---|---|---|---|
| 1 | Subscribe on AWS Marketplace | **Human, in a browser** | this skill, [below](#step-1--aws-marketplace-human-only) |
| 2 | Create the Union.ai organization | **Human, in a browser** | this skill, [below](#step-2--sign-up-and-create-the-organization-human-only) |
| 3 | Run a tracked workflow locally | Agent + human | `union-run-locally` |
| 4 | Provision the AWS resources | Agent, **gated on human approval** | `union-provision-aws` |
| 5 | Connect the cluster, install the agent | Agent + human, **hand-offs both ways** | `union-connect-cluster` |
| — | Something is unhealthy | Agent | `union-debug-cluster` |

Steps 1 and 2 are browser-only: there is no CLI for subscribing or for creating an
organization. Do not try to automate them. Step 3 is optional but is the cheapest way to
prove the organization works before spending money on AWS.

**Route the user.** Ask where they are before starting anything:

- *"I have nothing yet"* → step 1.
- *"I have an org but no cluster"* → offer step 3 (2 minutes, free) then step 4.
- *"I just want it running on my cluster"* → step 4, or step 5 if the AWS resources exist.
- *"I connected it and it says Unhealthy"* → `union-debug-cluster`. Note that **Unhealthy is
  expected for the first few minutes** after the agent connects; see that skill first.

---

## Shared conventions

These three conventions apply to **every** union skill. They exist because an agent is
driving a multi-step cloud deployment, which breaks two assumptions the docs make.

### 1. The state file — because your shell does not persist

The docs say "run every step in the same shell session" and use `export` to pass values
between steps. **An agent does not have one shell session.** Each command runs in a fresh
shell, so every `export` is lost the moment the command returns, and a later step that
reads `${CLUSTER_NAME}` silently gets an empty string — which is how you end up creating an
IAM role named `-system` or an S3 policy scoped to `arn:aws:s3:::`.

So: keep the values in a file, and source it at the top of **every** command.

```bash
# Default location. Override by exporting UNION_ENV_FILE before you start.
UNION_ENV_FILE="${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
```

Read at the start of every command, write as soon as a value is derived:

```bash
source "$UNION_ENV_FILE"                          # read
echo "export FOO=${FOO}" >> "$UNION_ENV_FILE"     # write (append-only; last one wins)
```

The file is append-only and the last definition of a name wins, so re-running a step is
safe. `.union-selfserve.env` is in this repo's `.gitignore`; if you create it somewhere
else, make sure it is ignored — it holds account IDs and role ARNs.

**Put `AWS_PROFILE` in it.** The same "fresh shell every command" problem that loses
`CLUSTER_NAME` also loses the AWS account: a profile the human exported earlier, or logged
into interactively, is gone by the next command, which then silently falls back to the
default profile. When the default is a shared or production account, that is how an EKS
cluster gets created next to production. Record it at the top of the file so every `source`
re-pins it:

```bash
echo "export AWS_PROFILE=<deployment-profile>" >> "$UNION_ENV_FILE"
```

Then confirm, every time it matters, that the account is the one you think:

```bash
source "$UNION_ENV_FILE"
aws sts get-caller-identity --query '[Account,Arn]' --output text
```

**One state file per generation.** If a previous attempt under a different `NAME_PREFIX` is
still around — being torn down, say — do not mix its names into the live file. Give the old
one its own file via `UNION_ENV_FILE=/some/other/path`, so a stale `CLUSTER_NAME` can never
be sourced into a command that deletes things.

**Guard every step.** Before doing anything that depends on prior state:

```bash
source "$UNION_ENV_FILE" 2>/dev/null || true
: "${CLUSTER_NAME:?run union-provision-aws step 1 first — the state file is missing or stale}"
```

`${VAR:?message}` aborts with your message instead of running the command with an empty
value. Use it for every variable a command interpolates into a resource name, an ARN, or
an IAM policy.

### 2. Approval gates — because these commands cost money

Commands are marked with one of two badges:

- **🟢 READ-ONLY** — describes, lists, gets, prints. Run it without asking.
- **⛔ APPROVAL GATE** — creates, modifies or deletes a cloud resource, or installs into a
  cluster. **Stop. Show the human the exact command, what it creates, and what it costs.
  Wait for an explicit "yes".** Do not batch several gated commands into one approval, and
  do not re-use an earlier approval for a re-run after a failure — a retry after a partial
  failure is exactly when the blast radius has changed.

Before the first gate in a session, state the running bill in plain terms: an EKS control
plane is ~$0.10/hour (~$73/month) *before* any nodes, EKS Auto Mode launches EC2 on
demand, and S3/ECR/NAT all bill separately. If the user is exploring rather than
committing, say that a dedicated test account makes teardown safe, and that
`union-run-locally` costs nothing.

### 3. UI hand-offs — because half the flow is a web form

Parts of this flow only exist in the browser. Mark each crossing explicitly so the human
knows a turn is theirs:

- **📤 TO THE UI** — the agent prints values; the human copies them into a Union.ai form.
  Print them as a labelled block, one field per line, named exactly as the form field is
  named. Never make the human hunt for a value in a wall of command output.
- **📥 FROM THE UI** — the human copies something out of the Union.ai UI and brings it
  back. Say precisely which button to click and what the thing looks like, so they can
  tell success from a half-copied buffer.
- **🔐 SECRET — DO NOT PRINT** — must not be echoed into the transcript, a file that gets
  committed, a ticket, or a chat message. The agent Helm values block in step 5 is one of
  these: it contains a client certificate and private key.

### 4. Status discipline — because provisioning is mostly waiting

Three things in this flow take minutes, not seconds: the EKS cluster (**15–20 min**), the
Auto Mode node for the first pod (**1–2 min**), and Union.ai installing the data plane
(**a few min**). Most confusion in a setup session comes from the human not knowing which
of those is in flight, whether it is stuck, or whose turn it is.

**Block, don't hand-poll.** Start one backgrounded waiter on the actual condition and pick
the work up when it returns. Do not re-check every turn, and do not chain `sleep` calls —
some harnesses block a bare `sleep` specifically to push you toward this:

```bash
until "<condition>"; do sleep 20; done; echo "READY"
```

**Keep a standing status table** whenever something is in flight. Three states, and never
drop the third:

| | |
|---|---|
| ✅ **Done** | with the concrete result — a name, an ARN, a status |
| ⏳ **In flight** | what it is and roughly how long it takes |
| ⏸ **Blocked on you** | the exact browser action needed, restated in full |

**Re-state blocked items every turn until they clear.** A hand-off mentioned once, ten
messages ago, has effectively not been made — and an item parked in the UI (a cluster still
registered, an old stack still billing) is invisible from the CLI. If the human changes
direction mid-flow, say plainly what that leaves outstanding rather than silently dropping
it.

**Say what is not yet real.** When a value is predictable before the resource exists — the
IRSA role ARNs are the standard case — give the value, then say in the same breath that the
resource does not exist yet and what has to happen first. Never let a correct-looking string
imply a working resource.

---

## Step 1 — AWS Marketplace (human only)

Union.ai bills through AWS Marketplace, so the charges land on the AWS bill. New
subscriptions start with a 30-day free trial.

1. Open the **[Union Team listing](https://aws.amazon.com/marketplace/pp/prodview-66k3cmidsgv5o)**
   and subscribe with the AWS account the charges should appear on.
2. Click **Set up your account** to hand off to Union.ai sign-up.

Three things to warn about, all of which are easier to prevent than to undo:

- **Union Team, not Union Enterprise.** Searching Marketplace for "Union.ai" returns both.
  Self-serve is Union Team. The link above goes to the right one.
- **The billing account need not be the deployment account.** Many organizations restrict
  Marketplace subscriptions to a procurement or billing account. That is fine — the data
  plane can run in a different AWS account entirely. Ask which account is which *now*, and
  record it, because `union-provision-aws` must run against the deployment account.
- **One subscription, one organization.** A subscription binds to exactly one Union.ai
  organization, permanently. Union.ai refuses a second rather than splitting the
  entitlement. So do not burn it on a throwaway org name.

## Step 2 — Sign up and create the organization (human only)

Go to **[signup.hosted.unionai.cloud](https://signup.hosted.unionai.cloud)** and continue
with a Google or Microsoft *work* account — those are the only identity providers offered.
(No subscription yet? The page has a **Sign up via AWS Marketplace** link back to step 1.)

Then create the organization, which is the top-level workspace holding projects, runs,
resources and team members:

| Field | Guidance |
|---|---|
| **Organization name** | Becomes the web address and is **permanent and globally unique**. Lowercase letters, digits, hyphens. The form checks availability as you type. |
| **Preferred Union region** | Where the *control plane* runs. Pick the region closest to where the data plane will live — this is not the same choice as `AWS_REGION` for your cluster, but they should be near each other. |

Setup takes about thirty seconds and shows five phases (receiving the request, creating the
organization, setting up sign-in, preparing the workspace, finalizing).

📥 **FROM THE UI — capture the organization endpoint.** When setup finishes you land on the
organization home page and the address bar reads `<your-org>.hosted.unionai.cloud`. Ask the
human for that hostname and record it before going any further; every later step needs it.

```bash
UNION_ENV_FILE="${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
touch "$UNION_ENV_FILE" && chmod 600 "$UNION_ENV_FILE"
echo "export UNION_ENDPOINT=<your-org>.hosted.unionai.cloud" >> "$UNION_ENV_FILE"
```

Record it **without** a `https://` scheme and without a trailing slash — `flyte create
config --endpoint` wants the bare hostname.

The home page then offers **Run something locally** and **Connect your cluster**. Those are
steps 3 and 4/5.

## Step 3 — Run a tracked workflow locally

→ **`union-run-locally`**. Installs the `flyte` CLI, writes a config pointing at the
organization, runs a workflow on the user's own machine with `--tracked`, and finds it under
**Tracked Runs** in the UI. No cluster, no AWS spend, ~2 minutes. Do this before step 4
unless the user objects: it proves the endpoint, the login and the org are all working, so a
later failure is unambiguously about the cluster.

## Step 4 — Provision the AWS resources

→ **`union-provision-aws`**. Creates an EKS cluster in Auto Mode, one S3 bucket, a private
ECR repository, and the two IRSA roles, then prints the six values the UI forms need.

Skippable if the user already has resources that meet the requirements — that skill opens
with the conformance checklist to test them against.

## Step 5 — Connect the cluster

→ **`union-connect-cluster`**. Creates the cluster pool and registers the cluster in the UI
(📤 TO THE UI), then installs the agent from a command the UI generates
(📥 FROM THE UI, carrying the one 🔐 SECRET in the flow). Union.ai installs the data plane through the agent's
outbound connection.

## When it does not come up

→ **`union-debug-cluster`**. Covers the agent not connecting, the data plane stalling, IRSA
trust mismatches, S3 and ECR denials, Auto Mode not scheduling, the Metrics Server
collision, and reading the install-progress panel correctly.

---

## Terminology, so the agent does not mix layers

| Term | What it is |
|---|---|
| **Control plane** | Union.ai's hosted service — UI, API, metadata. Lives in the *Union region* chosen at sign-up. You do not install it. |
| **Data plane** | Runs your workloads. Installed by Union.ai *into your EKS cluster*, in a namespace named `instance-<id>`. |
| **Agent** (`dp-agent`) | The small Helm release *you* install into namespace `dataplane-agent`. It dials out to Union.ai; the data plane arrives through it. Not to be confused with the AI agent running this skill, or with Flyte 1 "agents". |
| **Cluster pool** | A Union.ai grouping of clusters sharing one S3 bucket, secret store and image registry. Your first pool is always named `default`. |
| **Organization** | Your workspace at `<org>.hosted.unionai.cloud`. |
| **Tracked run** | A run executed on your own machine that reports to Union.ai (`flyte run --tracked`). Listed under **Tracked Runs**, separately from **Runs**. |

Union.ai is a commercial superset of open-source Flyte and uses the **same `flyte` SDK and
CLI** — `pip install flyte`, `flyte run`. There is no separate `union` CLI in v2. If you
find yourself reaching for `unionai`/`uctl`, that is the deprecated v1 tooling; stop.
