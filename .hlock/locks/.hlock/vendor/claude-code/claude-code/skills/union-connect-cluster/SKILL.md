---
name: union-connect-cluster
description: 'Connect an EKS cluster to Union.ai — create the AWS cluster pool, register the cluster with its system and task IAM role ARNs, install the dp-agent Helm release, and wait for Union.ai to install the data plane. Covers the copy-paste hand-offs in both directions and the credential in the generated values.yaml. Use for "connect my cluster", "create cluster pool", "install the Union agent", "dp-agent", or "Install Union on your AWS cluster".'
---

# Connect your cluster

You have an organization and the AWS resources. Connecting the cluster tells Union.ai where
to install the data plane — and then it installs it, through an outbound connection from an
agent you put in the cluster. Your code, data and credentials never leave your AWS account.

**Nothing dials in.** No port is opened, no endpoint exposed, no cluster credential handed
over. The `dp-agent` Helm release connects *out* to Union.ai and the data plane arrives
through that connection.

Docs: <https://www.union.ai/docs/v2/union/deployment/self-serve/connect-a-cluster/>

## The shape of this step

Half of it is a web form and half of it is a shell, so values cross between them twice:

```
  agent ─── values ──▶ human ──▶ Union.ai UI   §1, §2  four + two values, two forms
  Union.ai UI ──▶ human ─── command ──▶ shell  §3      the install command, and its key
  cluster ──▶ Union.ai ──▶ UI                  §4      install progress, nobody acts
```

Say which side of that the user is on at each point. The commonest way this step goes wrong
is the human not realising a turn is theirs and waiting on an agent that is waiting on them.

## What the agent needs from you

| Input | When | Notes |
|---|---|---|
| **The six values** | §1–§2 | From `union-provision-aws` step 8, or gathered for existing resources. Listed below. |
| **A name for the cluster in Union.ai** | §2 | **Permanent.** The agent must ask, not choose. Matching the EKS cluster name keeps the two from drifting. |
| **Clicking through two UI forms** | §1–§2 | There is no CLI for either. |
| **Copying the install command out of the UI** | §3 | Carries a private key — see [The credential](#the-credential) for how to move it without putting it in the transcript. |
| **Confirmation at the install gate** | §3 | The one ⛔ APPROVAL GATE in this skill. |

Everything after §3 is automatic: Union.ai installs the data plane through the agent's
outbound connection, and nobody has to do anything but wait.

---

## What you need

Six values, from
[`union-provision-aws` step 8](../union-provision-aws/SKILL.md#step-8--collect-the-values)
or gathered per
[references/existing-resources.md](../union-provision-aws/references/existing-resources.md):

| Value | Form | Shape |
|---|---|---|
| S3 bucket URI | Cluster pool | `s3://my-team-union-selfserve-metadata` — **with** the `s3://` scheme |
| AWS account ID | Cluster pool | 12 digits; the account holding Secrets Manager |
| AWS Region | Cluster pool | the Region of the cluster and Secrets Manager |
| Image registry URI | Cluster pool | `<account>.dkr.ecr.<region>.amazonaws.com/<repo>` — the **repository** URI, with the repo name |
| System IAM role ARN | Connect cluster | `arn:aws:iam::<account>:role/<prefix>-system` |
| Task IAM role ARN | Connect cluster | `arn:aws:iam::<account>:role/<prefix>-task` |

Plus:

- **Helm** installed, and
- **`kubectl` reaching the cluster** from the shell that will run the install — the same
  shell `aws eks update-kubeconfig` was run in.

🟢 READ-ONLY preflight. Run this before touching the UI, because discovering that `kubectl`
points at the wrong cluster *after* registering is annoying to unwind — a cluster's name in
Union.ai cannot be changed once connected.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}" 2>/dev/null || true
command -v helm >/dev/null || echo "MISSING: helm — https://helm.sh/docs/intro/install/"
kubectl config current-context
kubectl get nodes 2>&1 | head -3
kubectl get ns dataplane-agent 2>/dev/null && echo "NOTE: dataplane-agent namespace already exists — an agent may already be installed"
printf '%-22s %s\n' \
  METADATA_BUCKET     "${METADATA_BUCKET:-<MISSING>}" \
  AWS_ACCOUNT_ID      "${AWS_ACCOUNT_ID:-<MISSING>}" \
  AWS_REGION          "${AWS_REGION:-<MISSING>}" \
  IMAGE_REGISTRY      "${IMAGE_REGISTRY:-<MISSING>}" \
  SYSTEM_IAM_ROLE_ARN "${SYSTEM_IAM_ROLE_ARN:-<MISSING>}" \
  TASK_IAM_ROLE_ARN   "${TASK_IAM_ROLE_ARN:-<MISSING>}"
```

The context name should contain `CLUSTER_NAME`. If it does not, re-run
`aws eks update-kubeconfig --region "$AWS_REGION" --name "$CLUSTER_NAME"` before going on.
An empty node list is fine — Auto Mode has nothing to schedule yet.

---

## 1. Create the cluster pool

📤 **TO THE UI.** A cluster pool groups clusters that share one S3 bucket, secret store and
image registry. A new organization has none, so this comes first.

Print the four values for the human, exactly as the form labels them:

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
printf '%s\n' \
  "Create a cluster pool → AWS tab:" \
  "  S3 Bucket       s3://${METADATA_BUCKET}" \
  "  Account ID      ${AWS_ACCOUNT_ID}" \
  "  Region          ${AWS_REGION}" \
  "  Image registry  ${IMAGE_REGISTRY}"
```

Then walk them through it:

1. In the Union.ai UI, select **Connect your cluster** — on the home page, or in the sidebar.
2. The dialog reports that there is no cluster pool yet. Select **Create cluster pool**.
3. Select **AWS** and fill in the four fields above. **Pool name** is already filled in as
   `default` and the first pool is always called that — leave it.
4. Select **Create cluster pool**.

Two things that look like problems and are not:

- **No secret needs to exist in Secrets Manager.** The data plane creates and manages
  runtime secrets in that account and Region as it needs them. An empty Secrets Manager is
  the expected state here.
- The **Account ID** field is about *Secrets Manager*, which is why the docs label it that
  way. For a self-serve setup it is the same account as everything else.

## 2. Connect the cluster

📤 **TO THE UI.** When the pool is created, the dialog moves straight to **Connect a
cluster** with the new pool selected.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
printf '%s\n' \
  "Connect a cluster:" \
  "  Name                 <choose — see below>" \
  "  Namespace            union" \
  "  System IAM Role ARN  ${SYSTEM_IAM_ROLE_ARN}" \
  "  Task IAM Role ARN    ${TASK_IAM_ROLE_ARN}"
```

**Ask the human for the name before they type it.** It identifies the cluster in Union.ai
and **cannot be changed once connected**. Suggest the EKS cluster's own name so the two
never drift apart, and do not pick one for them.

### The Namespace field

The dialog asks for a **Namespace** — "The namespace Union is installed into on your
cluster" — warning that *"Your IAM or Workload Identity trust must name it."* That warning
makes people think they have to go back and edit IAM. **They do not**, if the roles came
from `union-provision-aws`.

Answer: **`union`**, the pre-filled default.

It works because both trust policies wildcard the *namespace* segment via `StringLike`:

```
system role : system:serviceaccount:*:union-system
              system:serviceaccount:*:flytepropeller-system
task role   : system:serviceaccount:*:default
              system:serviceaccount:*:union
```

The `*` is the namespace, so whatever is typed here matches. Verify rather than assert, if
there is any doubt about where the roles came from:

```bash
# 🟢 READ-ONLY
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
aws iam get-role --role-name "$SYSTEM_ROLE_NAME" \
  --query 'Role.AssumeRolePolicyDocument.Statement[0].Condition.StringLike' --output json
aws iam get-role --role-name "$TASK_ROLE_NAME" \
  --query 'Role.AssumeRolePolicyDocument.Statement[0].Condition.StringLike' --output json
```

If those come back with a **literal** namespace instead of `*` — someone narrowed them per
[iam-policies.md](../union-provision-aws/references/iam-policies.md), or the roles were
built by hand — then the field must match that literal namespace exactly.

> **Do not read the task role's `system:serviceaccount:*:union` as the namespace.** The
> trailing `union` there is a **service account** name. That it happens to match the default
> namespace value is coincidence, and mistaking one for the other leads people to "fix"
> working trust policies.

Check the two ARNs before they are submitted — swapping system for task produces a data
plane that installs and then fails at runtime in ways that do not point back here:

```bash
# 🟢 READ-ONLY
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
case "$SYSTEM_IAM_ROLE_ARN" in *-system) echo "system ARN OK";; *) echo "CHECK: system ARN does not end in -system";; esac
case "$TASK_IAM_ROLE_ARN"   in *-task)   echo "task ARN OK";;   *) echo "CHECK: task ARN does not end in -task";;   esac
```

Select **Connect cluster**. Union.ai registers the cluster and opens its page. **Registering
puts nothing on the cluster by itself** — nothing has been installed yet.

## 3. Install the agent

📥 **FROM THE UI**, and this is the one 🔐 **SECRET** in the whole flow.

The cluster's page — **Install Union on your AWS cluster** — shows an install command
generated for this specific cluster. The human clicks **Copy install command**. That command
is, in outline:

```bash
cat <<'UNION_DP_AGENT_VALUES' > values.yaml
# ...the values the UI generated for your cluster...
UNION_DP_AGENT_VALUES

helm upgrade --install dp-agent oci://ghcr.io/omnistrate/dataplane-agent-chart \
  --version "<chart-version>" \
  --namespace dataplane-agent --create-namespace --values values.yaml \
  --set nameOverride=dp-agent --timeout 10m0s --wait
```

> **This outline is not runnable.** It is here so you can recognise what the real command
> does and spot a truncated paste. The chart version and the entire values block come from
> the UI. Never reconstruct it by hand.

### Sanity-check the values without reading the secret

If the human already wrote `values.yaml` themselves and asks you to run the Helm command,
check three things first — all without printing the file:

```bash
# 🟢 READ-ONLY — never `cat values.yaml`
kubectl config current-context                       # MUST be the cluster you just registered
grep -E '^(cloudProvider|cloudRegion|replicas):' values.yaml
grep -ciE 'BEGIN (RSA |EC )?PRIVATE KEY|BEGIN CERTIFICATE' values.yaml   # expect >= 2
stat -f '%Sp %N' values.yaml 2>/dev/null || stat -c '%A %n' values.yaml
```

**Context first, always.** A kubeconfig that still has an *earlier* cluster's context current
is the single most damaging mistake available at this point — it installs the agent into the
wrong cluster and registers it against the wrong pool.

**`cloudProvider: byoc-onprem` and `cloudRegion: on-prem` are correct on an AWS cluster.**
This looks alarming — it reads as though the wrong provider was picked in the UI — but the
chart is only the outbound tunnel, and agent-connected clusters are modelled as on-prem BYOC
regardless of the underlying cloud. Corroborate it with `serviceAccount.annotations: {}`:
the agent gets no IRSA annotation because it needs no AWS access at all. Your AWS specifics
live in the cluster *pool* config from §1, not in this file. Do not "fix" this and do not
re-register the cluster over it.

**Fix the permissions before running.** The generated file lands world-readable while
holding a private key:

```bash
chmod 600 values.yaml
```

Keep it out of version control. If the working directory is (or becomes) a git repo, it
belongs in `.gitignore` next to `.union-selfserve.env`.

### The credential

> ⚠️ **The `values.yaml` block contains the agent's client certificate and private key.**
> It is the cluster's identity to Union.ai. Treat it like any other secret: it must not
> land in a ticket, a chat message, a shared document, a git commit — **or an agent
> transcript**, which is stored and often shared.

So the agent must **not** ask the human to paste that command into the conversation. Offer
these, in this order:

**A. The human runs it in their own terminal (default, safest).** Nothing sensitive touches
the transcript. Tell them: paste into a terminal where `kubectl config current-context`
points at this cluster, run it, and report back what it printed. Then continue at §4. In
Claude Code they can prefix with `!` to run it in-session — but note that puts the command
*and* the key into the transcript, so it is not an improvement here.

**B. Clipboard to file, without the agent ever seeing it.** The human copies from the UI and
the agent pipes the clipboard straight to a file, so the key is never rendered:

Use a **fixed** path, not `mktemp -d`: each agent command runs in a fresh shell, so a `$D`
captured here would be empty by the time the next command needs it.

```bash
umask 077
UNION_AGENT_DIR="${UNION_AGENT_DIR:-$PWD/.union-agent-install}"
mkdir -p "$UNION_AGENT_DIR" && chmod 700 "$UNION_AGENT_DIR"
( pbpaste 2>/dev/null || wl-paste 2>/dev/null || xclip -selection clipboard -o 2>/dev/null \
  || xsel --clipboard --output 2>/dev/null ) > "$UNION_AGENT_DIR/install.sh"

# 🟢 READ-ONLY — confirm the shape WITHOUT printing the contents:
wc -l "$UNION_AGENT_DIR/install.sh"
grep -c 'UNION_DP_AGENT_VALUES' "$UNION_AGENT_DIR/install.sh"           # expect 2
grep -c 'helm upgrade --install dp-agent' "$UNION_AGENT_DIR/install.sh" # expect 1
grep -Eo -- '--version [^ ]+' "$UNION_AGENT_DIR/install.sh"             # chart version, safe to show
```

Read `wc -l` first. A zero-length file means either the clipboard was empty or none of the
four clipboard tools is installed — on a headless Linux box none of them will be, and option
A is the answer. If `grep -c 'UNION_DP_AGENT_VALUES'` is not `2`, the copy was truncated;
have them click **Copy install command** again. **Never `cat install.sh`.**

Then ⛔ **APPROVAL GATE** — this installs a Helm release into the cluster. Run it from
inside that directory, because the script writes `values.yaml` beside itself:

```bash
UNION_AGENT_DIR="${UNION_AGENT_DIR:-$PWD/.union-agent-install}"
cd "$UNION_AGENT_DIR" && bash install.sh
```

**C. Last resort.** If neither works, the human may paste the command to the agent — but say
plainly first that the private key will then be in the transcript, and that the right
follow-up is to disconnect the cluster in the UI and re-register it to rotate the
credential. Do not pick this silently.

### Clean up afterwards

Whichever path was used, once Helm reports success:

```bash
UNION_AGENT_DIR="${UNION_AGENT_DIR:-$PWD/.union-agent-install}"
shred -u "$UNION_AGENT_DIR"/install.sh "$UNION_AGENT_DIR"/values.yaml 2>/dev/null || true
rm -rf "$UNION_AGENT_DIR"
```

The credential now lives in a Kubernetes Secret in the cluster, which is where it belongs.
The file on disk is a second copy with no purpose.

### What "success" looks like

The command ends with `--wait`, so Helm blocks until the agent's pods are ready and then
prints `STATUS: deployed`. 🟢 READ-ONLY:

```bash
helm -n dataplane-agent status dp-agent --output json | jq -r '.info.status, .info.description'
kubectl -n dataplane-agent get pods
```

A Helm timeout is not necessarily a failed install — see `union-debug-cluster`, and check
whether the pods came up anyway before reinstalling.

## 4. Wait for the data plane

No human action. The cluster page's **Installation progress** panel tracks two phases:

| Phase | What is happening | Who acts |
|---|---|---|
| **Installing the Union agent** | Your `dp-agent` release starts and dials out to Union.ai. The panel shows the connection status. | you, in §3 |
| **Installing the Union operator** | Union.ai pushes the data plane down that connection. The panel shows cluster health. | nobody |

**The cluster shows as Unhealthy at the top of the page during this, and that is expected.**
It is not a symptom. The second phase takes a few minutes on a new cluster, mostly spent
pulling images and — on an Auto Mode cluster with no nodes yet — waiting for EC2 instances
to launch for the first time. The human can leave the page and come back.

Watching from the cluster side, 🟢 READ-ONLY. **List pods across all namespaces**: the data
plane installs into its own namespace named `instance-` followed by an identifier, which
does not exist until Union.ai creates it.

```bash
kubectl get pods -A | grep -Ev '^(kube-system|kube-public|kube-node-lease)' | head -40
kubectl get ns -o name | grep '^namespace/instance-' || echo "(data plane namespace not created yet)"
kubectl get nodes    # Auto Mode nodes should start appearing now that there are pods
```

**The agent pod sitting `Pending` with `kubectl get nodes` empty is the expected first
state, not a failure.** On a fresh Auto Mode cluster the agent is the first workload, so
there are no nodes until EKS provisions one for it — typically 60–120 seconds, after which
the pod schedules on its own. Say this *before* the human sees `Pending` and concludes
something broke. This is also where EC2 charges genuinely begin; the control plane has been
billing since it was created.

Rather than hand-polling each turn, block on the data plane namespace appearing:

```bash
# backgrounded — returns when Union.ai has created the data plane namespace
until kubectl get ns -o name 2>/dev/null | grep -q '^namespace/instance-'; do sleep 20; done
echo "DATA PLANE NAMESPACE CREATED"
kubectl get ns -o name | grep '^namespace/instance-'
```

Poll at roughly 30-second intervals if you must do it by hand, and give the human a one-line
status each time rather than dumping full pod tables. Done when the panel reads **Complete**
and the badge next to the cluster's name on its own page turns green and reads **Healthy** —
that badge is the one that was showing **Unhealthy** all through the install, so tell the
human to watch it there rather than going back to the cluster list.

Still not Complete after ~15 minutes, or pods in `CrashLoopBackOff`, `ImagePullBackOff` or
`Pending` → **`union-debug-cluster`**.

## 5. Run a workload on the cluster

The payoff: the same file from `union-run-locally`, **without** `--tracked`.

```bash
flyte run hello.py main
```

It now executes on your cluster instead of on the user's machine, and appears under
**Runs** in the project — *not* under **Tracked Runs**, which is only for runs that
executed locally. Point that out; seeing the same code land in a different section is the
clearest demonstration of what connecting the cluster bought.

The first cluster run is slower than later ones: the image has to be built and pushed to
ECR, and Auto Mode may need to launch a node for it.

If this is the first `flyte` use on this machine, set up the CLI first — see
`union-run-locally` §1–3.

---

## After this

Setup is done. Union.ai runs your workloads on your own infrastructure.

- Build workflows → [Get started](https://www.union.ai/docs/v2/union/user-guide/get-started.md),
  or the `flyte-sdk-*` skills if they are installed.
- Tighten the IAM trust now that the release namespace exists →
  [union-provision-aws references/iam-policies.md](../union-provision-aws/references/iam-policies.md#narrowing-after-the-install).
- Before production: S3 and ECR lifecycle rules, and an image-expiry policy. Nothing in this
  setup expires anything.

## Reconnecting or reinstalling

- **Re-running the install command** is safe — it is `helm upgrade --install`, and the values
  are generated for this cluster.
- **A new `values.yaml` from the UI supersedes the old one.** Do not mix blocks from two
  copies; copy the whole command fresh.
- **Disconnecting a cluster in the UI invalidates its agent credential.** Re-registering
  produces a new name, a new credential, and a new `instance-<id>` namespace — so if you
  narrowed the system role's trust policy to a specific namespace, revisit it.
- **The cluster's name in Union.ai is permanent.** Changing it means disconnect and
  re-register.
