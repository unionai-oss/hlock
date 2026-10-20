---
name: union-provision-aws
description: 'Provision the AWS resources a Union.ai self-serve cluster pool needs — an EKS cluster in Auto Mode, an S3 metadata bucket, a private ECR repository, and the system and task IRSA roles — then print the six values the Union.ai UI asks for. Every resource-creating command is an explicit approval gate. Use for "provision AWS for Union", "Union EKS cluster", "self-serve AWS infrastructure", "create the IRSA roles", or before union-connect-cluster.'
---

# Provision your AWS resources

Union.ai installs the data plane into your cluster for you, but the AWS resources it runs
on must exist first. This skill creates them:

| Resource | What it is for |
|---|---|
| **EKS cluster**, Auto Mode | Runs the data plane. Auto Mode adds and removes nodes as pods need them, so there is no node group to size and no autoscaler to install. |
| **One S3 bucket** | The cluster pool's object store: workflow metadata, task inputs and outputs, artifacts, and fast-registration code bundles. |
| **Private ECR repository** | Where task images are pushed and pulled. |
| **System IRSA role** | Assumed by the data plane's own service accounts. |
| **Task IRSA role** | Assumed by task pods in project namespaces. |

The output is six values you type into two Union.ai forms in `union-connect-cluster`.

Docs: <https://www.union.ai/docs/v2/union/deployment/self-serve/aws-infrastructure/>

> **This is the self-serve shape, not the manual self-managed one.** Runtime secrets live in
> AWS Secrets Manager, IAM trust follows the namespace the agent picks, and the data plane
> chart installs its own Metrics Server. The manual
> [AWS infrastructure guide](https://www.union.ai/docs/v2/union/deployment/selfmanaged/infrastructure-recommendations/aws.md)
> describes different resources. Do not mix the two.

## Already have AWS resources?

This whole skill is optional. An existing EKS cluster, bucket, repository and roles work
if they meet the requirements — read **[references/existing-resources.md](references/existing-resources.md)**,
check them against it, then collect the [step 8](#step-8--collect-the-values) values and go
to `union-connect-cluster`.

## What the agent needs from you

Ask for all of these **before** running anything, and read them back. They are cheap to
change now and expensive to change after step 2.

| Input | Why it matters | Changeable later? |
|---|---|---|
| **Which AWS account** | May not be the Marketplace billing account. Everything lands here. | No — you would rebuild. |
| **`AWS_REGION`** | Where the cluster and its Secrets Manager secrets live. Put it near the **Union region** chosen at sign-up. | No. |
| **`NAME_PREFIX`** | Every resource name derives from it. | No. |
| **Bucket name** | Derived from the prefix, but **globally unique across all of AWS**. A collision fails step 3 and is not retryable without a new name. | No. |
| **Kubernetes version** | Must be one EKS offers in this region today. [Step 1](#step-1--names-and-identity) checks. | Yes — cluster upgrade. |
| **Confirmation at each ⛔ APPROVAL GATE** | Ten of them, across eight steps. Steps 2, 3 and 4 are the ones that start charges; the rest create IAM objects, which are free. | — |

You do **not** need a Secrets Manager secret, a fast-registration bucket, a node group, or
an autoscaler. Those either come later or do not apply — see
[Behaviour worth remembering](#behaviour-worth-remembering).

---

## Operating rules for this skill

**Every command below carries a badge.** 🟢 READ-ONLY commands run freely. ⛔ APPROVAL GATE
commands create, modify or delete AWS resources — show the human the exact command and what
it will create, and wait for an explicit "yes". One gate, one approval: do not batch, and do
not reuse an approval when retrying after a failure.

**Money.** Before the first gate, say this plainly: the EKS control plane bills about
**$0.10/hour (~$73/month) from the moment it exists, with zero nodes running**; Auto Mode
launches EC2 instances on demand and bills them; the VPC `eksctl` creates includes NAT
gateways at roughly **$0.045/hour each plus data processing**; S3 and ECR bill for what they
store. A dedicated test account makes teardown unambiguous. Nothing in this skill is free.

**The state file.** Each command starts by sourcing it and asserting what it needs, because
an agent's shell does not survive between commands and an empty `${CLUSTER_NAME}` produces
an IAM role literally named `-system`:

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${CLUSTER_NAME:?state file missing or stale — re-run step 1}"
```

**Idempotency.** Every step checks for its resource before creating it. Assume any step may
be re-run: agents retry, sessions get interrupted, and `eksctl create cluster` failing at
minute 14 leaves a half-built CloudFormation stack that a blind retry will not fix.

---

## Prerequisites

🟢 READ-ONLY — run this first and read the output before anything else.

```bash
# Each tool reports its version differently; ask each one the way it expects.
for spec in "aws:--version" "eksctl:version" "kubectl:version --client -o yaml" \
            "jq:--version" "helm:version --short"; do
  tool=${spec%%:*}; flags=${spec#*:}
  printf '%-8s ' "$tool"
  if command -v "$tool" >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    "$tool" $flags 2>/dev/null | grep -Eom1 '[0-9]+\.[0-9]+(\.[0-9]+)?' || echo "(installed)"
  else
    echo "MISSING"
  fi
done
echo "--- identity ---"
aws sts get-caller-identity
echo "--- region ---"
aws configure get region || echo "(no default region configured)"
echo "--- profiles ---"
aws configure list-profiles 2>/dev/null | head -40
```

An expired SSO token shows up here as
`Error when retrieving token from sso: Token has expired and refresh failed`. The agent
cannot fix that — logging in is interactive. Ask the human to run `aws sso login`
themselves (in Claude Code, `! aws sso login` runs it in-session), then re-run the check.

Required:

- **AWS CLI v2**, authenticated against the **deployment** account — which may not be the
  Marketplace billing account. Check the account number in `get-caller-identity` against
  what the user expects and say it back to them. Deploying into the wrong account is
  discovered late and cleaned up slowly.
- **`eksctl` 0.195.0 or later.** Auto Mode support is the reason for the floor; an older
  `eksctl` will reject `autoModeConfig` or silently produce a non-Auto-Mode cluster.
- **`kubectl`**, **`jq`**, and **`helm`** (helm is used by `union-connect-cluster`, but
  find out now rather than 30 minutes in).
- Permissions to create EKS, EC2/VPC, IAM, S3, ECR and CloudWatch resources. A scoped role
  that cannot create IAM roles will get all the way to step 5 before failing.

`envsubst` is **not** required here. The docs use it to fill variables into the IAM policy
documents; the commands below use a plain unquoted heredoc instead, which does the same
substitution with nothing to install. This matters because `envsubst` ships with GNU gettext
and is absent from a stock macOS.

### Pin the account with `AWS_PROFILE`, not with whatever is default

Many people's default profile points at a **shared** account that also holds production.
Every agent command runs in a fresh shell, so "I switched profiles earlier" does not
survive — the next command silently falls back to the default account and creates an EKS
cluster next to production.

So: decide the deployment profile now, and record it **in the state file** (step 1) so every
`source` re-pins it. Never carry it as a bare `--profile` flag you have to remember.

```bash
# 🟢 READ-ONLY — confirm the profile maps to the account you expect
AWS_PROFILE=<deployment-profile> aws sts get-caller-identity --query '[Account,Arn]' --output text
```

> **zsh gotcha, worth knowing before it bites.** Do *not* stash the flag in a variable and
> rely on word-splitting — `P="--profile foo"; aws $P eks list-clusters` works in bash but
> **fails in zsh**, which does not word-split unquoted expansions by default, so the whole
> string arrives as one argument: `Found invalid choice '--profile foo'`. Export
> `AWS_PROFILE` instead; it needs no splitting and `eksctl`, `kubectl` and `helm` all honour
> it too.

### Sweep for earlier generations before you spend anything

🟢 READ-ONLY. People arrive at this skill having already half-built a stack under a previous
name — and an abandoned EKS cluster bills ~$73/month plus nodes and NAT gateways
indefinitely. Find it **before** adding a second one.

```bash
aws eks list-clusters --region "${AWS_REGION:-us-east-2}" --output text
aws s3api list-buckets --query 'Buckets[].Name' --output text | tr '\t' '\n' | grep -i union
aws ecr describe-repositories --region "${AWS_REGION:-us-east-2}" \
  --query 'repositories[].repositoryName' --output text 2>/dev/null
aws iam list-roles --query 'Roles[?contains(RoleName, `union`)].RoleName' --output text
```

If something from an earlier attempt turns up, **say what it costs and ask** before
provisioning alongside it. A cluster that is `ACTIVE` with running nodes and a
`dataplane-agent` namespace is a *connected* cluster: it may still be registered in
Union.ai, and [references/teardown.md](references/teardown.md) has to start in the UI, not
in the CLI.

```bash
# 🟢 READ-ONLY — is that old cluster still connected?
kubectl --context "<old-context>" get ns 2>/dev/null | grep -E 'dataplane-agent|instance-'
```

---

## Step 1 — Names and identity

⛔ APPROVAL GATE — this writes the state file (and nothing else), but the names it fixes are
permanent for the rest of the flow. **Confirm every value with the human before running it.**

Ask for, do not assume:

- `NAME_PREFIX` — their team or project slug. Everything else is derived from it.
- `AWS_REGION` — where the cluster and its Secrets Manager secrets live. Should be close to
  the **Union region** chosen at sign-up (a different choice — the control plane's region).
- The bucket name, which is **globally unique across all of AWS**, not just this account.
  A collision here fails at step 3 with `BucketAlreadyExists` and is not retryable without
  a different name.

```bash
UNION_ENV_FILE="${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
touch "$UNION_ENV_FILE" && chmod 600 "$UNION_ENV_FILE"

cat >> "$UNION_ENV_FILE" <<'EOF'
# Pins every later command to the deployment account. Without this, a fresh shell
# falls back to the default profile — which is often a shared or production account.
# Omit the line only if the default profile IS the deployment account.
export AWS_PROFILE=<deployment-profile>
export AWS_REGION=us-east-2
export NAME_PREFIX=<my-team>
export KUBERNETES_VERSION=1.34
export CLUSTER_NAME=${NAME_PREFIX}-union-selfserve
export BUCKET_PREFIX=${NAME_PREFIX}-union-selfserve
export METADATA_BUCKET=${BUCKET_PREFIX}-metadata
export ECR_REPO_NAME=${CLUSTER_NAME}
export SYSTEM_ROLE_NAME=${CLUSTER_NAME}-system
export TASK_ROLE_NAME=${CLUSTER_NAME}-task
# "*" matches the release namespace the agent picks, which is not known until the
# data plane is installed. Narrow it only after the cluster is connected and the
# namespace is stable — see references/iam-policies.md.
export DATAPLANE_NAMESPACE='*'
EOF
```

Replace `<my-team>` and `us-east-2` **before** running that. Then resolve the account and
confirm the derived names:

```bash
source "$UNION_ENV_FILE"
: "${NAME_PREFIX:?}"; case "$NAME_PREFIX" in *"<"*) echo "NAME_PREFIX placeholder not replaced" >&2; exit 1;; esac
echo "export AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)" >> "$UNION_ENV_FILE"
source "$UNION_ENV_FILE"
printf 'account=%s region=%s\ncluster=%s\nbucket=%s\necr=%s\nroles=%s , %s\n' \
  "$AWS_ACCOUNT_ID" "$AWS_REGION" "$CLUSTER_NAME" "$METADATA_BUCKET" \
  "$ECR_REPO_NAME" "$SYSTEM_ROLE_NAME" "$TASK_ROLE_NAME"
```

`KUBERNETES_VERSION` above is a starting point, not a guarantee — the docs' example pins
`1.35` and AWS retires versions on its own schedule. Check what this region actually offers
before relying on either value:

```bash
# 🟢 READ-ONLY
aws eks describe-cluster-versions --region "$AWS_REGION" --output json 2>/dev/null \
  | jq -r '.clusterVersions[]? | "\(.clusterVersion)\t\(.clusterVersionStatus)"' \
  || echo "(older AWS CLI: check the EKS console for supported versions)"
```

The filtered `--query 'clusterVersions[?clusterVersionStatus==\`standard-support\`]'` form
prints **nothing at all** on some CLI versions, because `clusterVersionStatus` comes back
`null` — which reads exactly like "no versions available" and sends you chasing a
non-problem. List every version with its status and decide by eye.

If `KUBERNETES_VERSION` is not in that list, append a corrected `export KUBERNETES_VERSION=`
line to the state file rather than editing in place — last definition wins.

Confirm the names are free before spending 20 minutes:

```bash
# 🟢 READ-ONLY — "not found" errors here are the good outcome
aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" --query 'cluster.status' --output text 2>&1 | tail -1
aws s3api head-bucket --bucket "$METADATA_BUCKET" 2>&1 | tail -1
aws ecr describe-repositories --region "$AWS_REGION" --repository-names "$ECR_REPO_NAME" --query 'repositories[0].repositoryUri' --output text 2>&1 | tail -1
aws iam get-role --role-name "$SYSTEM_ROLE_NAME" --query 'Role.Arn' --output text 2>&1 | tail -1
aws iam get-role --role-name "$TASK_ROLE_NAME" --query 'Role.Arn' --output text 2>&1 | tail -1
```

A bucket that exists but is owned by another AWS account returns `403 Forbidden`, not
`404` — that name is taken globally and must be changed.

---

## Run the steps in this order, not in numbered order

The numbering follows the docs. The *useful* order is different, and it matters because the
EKS build is 15–20 minutes of dead time the human otherwise spends staring at a spinner.

**The cluster pool form needs only four values — S3 bucket, account ID, Region, image
registry — and not one of them depends on the EKS cluster.** So build the cheap resources
first, hand the human their four values, and let them fill in the UI *while* the cluster
builds:

| Order | Step | Why here |
|---|---|---|
| 1 | **Step 3** — S3 bucket + CORS | Seconds. Costs ~nothing empty. |
| 2 | **Step 4** — ECR repository | Seconds. Costs ~nothing empty. |
| 3 | 📤 **TO THE UI** — the four pool values | Human starts the UI form now, not in 20 minutes. |
| 4 | **Step 2** — EKS cluster, **backgrounded** | The long pole. Runs while they click. |
| 5 | **Step 2a/2b** — OIDC, kubeconfig | Needs the cluster. |
| 6 | **Steps 5–7** — roles, policies, ECR policy | Needs the OIDC issuer. |
| 7 | 📤 **TO THE UI** — the two role ARNs | Human finishes the *Connect cluster* dialog. |

Tell the human this shape up front. "Two of your four values are ready now, the other two
in about twenty minutes" sets a very different expectation from silence.

### The role ARNs are predictable — the roles are not

The human will ask for the two role ARNs early, often repeatedly, because the UI asks for
them. Their **values** are fully determined by step 1:

```
arn:aws:iam::${AWS_ACCOUNT_ID}:role/${CLUSTER_NAME}-system
arn:aws:iam::${AWS_ACCOUNT_ID}:role/${CLUSTER_NAME}-task
```

Give them those strings when asked, **with an explicit warning that the roles do not exist
yet**, and say why rather than appearing to stall: the whole substance of an IRSA role is a
trust policy naming the cluster's OIDC issuer, and that issuer ID is minted with the
cluster. Creating the roles early would mean either a placeholder trust policy nothing can
assume, or a malformed federated principal that **IAM accepts without complaint** and that
never works — surfacing much later as pods that cannot reach S3.

If the UI validates the ARN on submit, it will reject them until step 5 runs. Say so.

---

## Step 2 — The EKS cluster

⛔ **APPROVAL GATE — the largest single commitment in this skill.** Creating an EKS cluster
takes **15–20 minutes** and starts the control-plane charge immediately. It also creates a
VPC, subnets, NAT gateways and the Auto Mode node IAM role. Get an explicit yes.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${CLUSTER_NAME:?}"; : "${AWS_REGION:?}"; : "${KUBERNETES_VERSION:?}"

eksctl create cluster --config-file <(
  jq -n \
    --arg clusterName "${CLUSTER_NAME}" \
    --arg region "${AWS_REGION}" \
    --arg version "${KUBERNETES_VERSION}" \
    '{
      apiVersion: "eksctl.io/v1alpha5",
      kind: "ClusterConfig",
      metadata: { name: $clusterName, region: $region, version: $version },
      autoModeConfig: { enabled: true, nodePools: ["general-purpose", "system"] },
      addonsConfig: { disableDefaultAddons: true }
    }'
)
```

Two parts of that config are load-bearing:

- **`autoModeConfig`** puts the cluster in [EKS Auto Mode](https://docs.aws.amazon.com/eks/latest/userguide/automode.html).
  Nodes appear when pods need them and go away when idle. Nothing to size, no Karpenter or
  cluster-autoscaler to install.
- **`disableDefaultAddons: true`** keeps EKS from installing the Metrics Server add-on.
  **The data plane chart installs Metrics Server itself**, and two of them fight over the
  same cluster-scoped resources. Auto Mode supplies networking and DNS, so the default
  add-ons are not missed. Do not "fix" this later by adding the EKS Metrics Server add-on.

**Run it backgrounded.** It routinely outruns a foreground tool timeout, and backgrounding
frees the human to fill in the cluster pool form meanwhile. 20 minutes of silence is normal,
not a hang.

### Block until the cluster is ready, instead of polling by hand

Do **not** hand-poll `describe-cluster` every turn, and do not chain `sleep` calls — some
agent harnesses refuse a bare `sleep` precisely to push you here. Start one backgrounded
waiter that returns the moment the cluster flips to `ACTIVE`, and pick the work back up when
it fires:

```bash
# backgrounded — returns only when ACTIVE, then prints everything step 2a needs
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
until [ "$(aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
           --query 'cluster.status' --output text 2>/dev/null)" = "ACTIVE" ]; do sleep 20; done
echo "CLUSTER ACTIVE"
aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
  --query 'cluster.{status:status,version:version,autoMode:computeConfig.enabled,oidc:identity.oidc.issuer,nodeRole:computeConfig.nodeRoleArn}' \
  --output json
```

Two practical notes:

- **Pipe the `eksctl` build through `tail` and you lose interim progress** — the pipe buffers
  until exit. That is fine, but it means the build log tells you nothing mid-flight; read
  progress from CloudFormation instead:
  `aws cloudformation describe-stacks --region "$AWS_REGION" --query "Stacks[?starts_with(StackName, 'eksctl-${CLUSTER_NAME}')].[StackName,StackStatus]" --output text`
- **The stack takes a minute or two to appear at all.** An empty result right after launch is
  `eksctl` preflight, not a failure. Auto Mode means there is no second nodegroup stack to
  wait on afterwards — when the cluster stack completes, the cluster is ready.

While it builds, keep the human oriented: what is already done, what is in flight, what is
still blocked on them. A status table beats a progress bar here.

If it fails partway, **do not blind-retry**:
`eksctl` leaves CloudFormation stacks behind. Inspect and delete first:

```bash
# 🟢 READ-ONLY
aws cloudformation describe-stacks --region "$AWS_REGION" \
  --query "Stacks[?starts_with(StackName, 'eksctl-${CLUSTER_NAME}')].[StackName,StackStatus]" --output table
# then, gated: eksctl delete cluster --name "$CLUSTER_NAME" --region "$AWS_REGION" --wait
```

### Step 2a — OIDC provider and derived values

⛔ APPROVAL GATE (the first command creates an IAM OIDC provider; it is a no-op if one
already exists).

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
eksctl utils associate-iam-oidc-provider --cluster "${CLUSTER_NAME}" --region "${AWS_REGION}" --approve
```

Then derive and persist the two values every later IAM policy interpolates. 🟢 READ-ONLY:

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"

OIDC_PROVIDER=$(aws eks describe-cluster --region "${AWS_REGION}" --name "${CLUSTER_NAME}" \
  --query 'cluster.identity.oidc.issuer' --output text | sed 's|https://||')
NODE_ROLE_ARN=$(aws eks describe-cluster --region "${AWS_REGION}" --name "${CLUSTER_NAME}" \
  --query 'cluster.computeConfig.nodeRoleArn' --output text)

case "${OIDC_PROVIDER:-None}" in None|"") echo "FAILED to resolve OIDC_PROVIDER" >&2; exit 1;; esac
case "${NODE_ROLE_ARN:-None}" in None|"") echo "FAILED to resolve NODE_ROLE_ARN" >&2; exit 1;; esac
{ echo "export OIDC_PROVIDER=${OIDC_PROVIDER}"; echo "export NODE_ROLE_ARN=${NODE_ROLE_ARN}"; } >> "$UNION_ENV_FILE"
echo "OIDC_PROVIDER=${OIDC_PROVIDER}"; echo "NODE_ROLE_ARN=${NODE_ROLE_ARN}"
```

**Stop if either is empty or `None`.** `OIDC_PROVIDER` empty produces a trust policy with a
malformed federated principal that IAM may accept and that will never let anything assume
the role; `NODE_ROLE_ARN` empty means the cluster is not in Auto Mode, which is a different
and bigger problem. Neither failure surfaces until pods are already crash-looping in
step 5 of `union-connect-cluster`.

### Step 2b — kubeconfig and cluster health

⛔ APPROVAL GATE (light — it edits `~/.kube/config`, which may hold other clusters' contexts).

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
aws eks update-kubeconfig --region "${AWS_REGION}" --name "${CLUSTER_NAME}"
```

🟢 READ-ONLY verification:

```bash
kubectl get namespaces
kubectl get nodes
if kubectl get clusterrole system:metrics-server-aggregated-reader >/dev/null 2>&1; then
  echo "ERROR: an existing Metrics Server will collide with the data plane chart." >&2
else
  echo "OK: no Metrics Server collision."
fi
```

- `kubectl get namespaces` must list `kube-system` and friends. An `Unauthorized` here means
  the identity that created the cluster is not an EKS administrator — grant its IAM role the
  `AmazonEKSClusterAdminPolicy` access entry, then re-run `update-kubeconfig`.
- **`kubectl get nodes` showing no nodes is correct at this point.** Auto Mode starts nodes
  when there are pods to run, and there are none yet. Do not go hunting for a node group.
- The Metrics Server check must print OK. If it does not, resolve it now —
  `union-debug-cluster` has the removal path.

---

## Step 3 — The S3 bucket

⛔ APPROVAL GATE.

Note the region special case: **`us-east-1` must omit `--create-bucket-configuration`
entirely** (passing `LocationConstraint=us-east-1` is an error). The guard below handles it.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${METADATA_BUCKET:?}"; : "${AWS_REGION:?}"

if aws s3api head-bucket --bucket "${METADATA_BUCKET}" 2>/dev/null; then
  echo "bucket ${METADATA_BUCKET} already exists and is owned by this account — skipping create"
elif [ "${AWS_REGION}" = "us-east-1" ]; then
  aws s3api create-bucket --bucket "${METADATA_BUCKET}" --region us-east-1
else
  aws s3api create-bucket --bucket "${METADATA_BUCKET}" --region "${AWS_REGION}" \
    --create-bucket-configuration "LocationConstraint=${AWS_REGION}"
fi

aws s3api put-public-access-block --bucket "${METADATA_BUCKET}" \
  --public-access-block-configuration \
  'BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true'
```

Then the CORS rule, which lets the Union.ai UI fetch code and artifacts through presigned
URLs. Without it, runs work but the UI's data and code views fail with opaque browser
errors — a symptom that is very hard to trace back to a missing bucket CORS policy.

⛔ APPROVAL GATE:

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
aws s3api put-bucket-cors --bucket "${METADATA_BUCKET}" --cors-configuration '{
  "CORSRules": [
    {
      "AllowedHeaders": ["*"],
      "AllowedMethods": ["GET", "PUT", "POST", "DELETE", "HEAD"],
      "AllowedOrigins": ["https://*.unionai.cloud", "https://*.union.ai"],
      "ExposeHeaders": ["ETag"],
      "MaxAgeSeconds": 3600
    }
  ]
}'
```

If the UI is served from a domain that is not a Union.ai one, add that hostname to
`AllowedOrigins`.

**One bucket, both jobs.** Union.ai points the data plane at this bucket for metadata *and*
for fast registration. Do not create a separate fast-registration bucket; there is no field
for one and a second bucket just drifts out of sync.

For production, add a lifecycle policy, the encryption and KMS controls your organization
requires, and a retention policy suited to workflow data. This skill configures none of
those.

---

## Step 4 — The ECR repository

⛔ APPROVAL GATE.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${ECR_REPO_NAME:?}"

if aws ecr describe-repositories --region "${AWS_REGION}" \
     --repository-names "${ECR_REPO_NAME}" >/dev/null 2>&1; then
  echo "repository ${ECR_REPO_NAME} already exists — skipping create"
else
  aws ecr create-repository --repository-name "${ECR_REPO_NAME}" --region "${AWS_REGION}" \
    --image-scanning-configuration scanOnPush=true
fi

IMAGE_REGISTRY=$(aws ecr describe-repositories --repository-names "${ECR_REPO_NAME}" \
  --region "${AWS_REGION}" --query 'repositories[0].repositoryUri' --output text)
: "${IMAGE_REGISTRY:?failed to resolve the repository URI}"
echo "export IMAGE_REGISTRY=${IMAGE_REGISTRY}" >> "$UNION_ENV_FILE"
echo "IMAGE_REGISTRY=${IMAGE_REGISTRY}"
```

`IMAGE_REGISTRY` is one of the four cluster-pool values and has the form
`<account-id>.dkr.ecr.<region>.amazonaws.com/<repository-name>`. It is the **repository**
URI, not the registry host — the `/<repository-name>` suffix belongs in it.

---

## Step 5 — The two IRSA roles

⛔ APPROVAL GATE. Creates two IAM roles with trust policies bound to the cluster's OIDC
provider.

Who assumes what:

- **System role** — the data plane's own service accounts, `union-system` and the legacy
  `flytepropeller-system`, in the data plane namespace.
- **Task role** — task-pod service accounts (`default` or `union`) in the per-project
  namespaces the data plane creates dynamically.

The wildcards are deliberate. `DATAPLANE_NAMESPACE` is `*` because the release namespace is
`instance-<id>` and the identifier is not known until Union.ai installs the data plane; the
task role trusts `*` because project namespaces are created on demand. See
[references/iam-policies.md](references/iam-policies.md) for the exposure this implies and
how to narrow it afterwards.

The heredoc delimiter is **unquoted**, so the shell expands `$AWS_ACCOUNT_ID`,
`$OIDC_PROVIDER` and `$DATAPLANE_NAMESPACE` into the policy. That is the `envsubst`
substitution from the docs, without the dependency.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${AWS_ACCOUNT_ID:?}"; : "${OIDC_PROVIDER:?}"; : "${SYSTEM_ROLE_NAME:?}"; : "${TASK_ROLE_NAME:?}"
: "${DATAPLANE_NAMESPACE:?}"

SYSTEM_TRUST=$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": { "Federated": "arn:aws:iam::${AWS_ACCOUNT_ID}:oidc-provider/${OIDC_PROVIDER}" },
    "Action": "sts:AssumeRoleWithWebIdentity",
    "Condition": {
      "StringEquals": { "${OIDC_PROVIDER}:aud": "sts.amazonaws.com" },
      "StringLike": { "${OIDC_PROVIDER}:sub": [
        "system:serviceaccount:${DATAPLANE_NAMESPACE}:union-system",
        "system:serviceaccount:${DATAPLANE_NAMESPACE}:flytepropeller-system"
      ]}
    }
  }]
}
EOF
)

TASK_TRUST=$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": { "Federated": "arn:aws:iam::${AWS_ACCOUNT_ID}:oidc-provider/${OIDC_PROVIDER}" },
    "Action": "sts:AssumeRoleWithWebIdentity",
    "Condition": {
      "StringEquals": { "${OIDC_PROVIDER}:aud": "sts.amazonaws.com" },
      "StringLike": { "${OIDC_PROVIDER}:sub": [
        "system:serviceaccount:*:default",
        "system:serviceaccount:*:union"
      ]}
    }
  }]
}
EOF
)

# Sanity-check before IAM sees them: unexpanded variables here become a role nothing
# can ever assume, and IAM will accept it without complaint.
echo "$SYSTEM_TRUST" | jq -e '.Statement[0].Principal.Federated | test("oidc-provider/oidc\\.eks\\.")' >/dev/null \
  || { echo "system trust policy looks wrong — check OIDC_PROVIDER" >&2; exit 1; }

# if/then/else, not `A && B || C`: with the latter, an update that FAILS on an
# existing role would fall through to create-role and fail again with a
# misleading EntityAlreadyExists, hiding the real error.
if aws iam get-role --role-name "${SYSTEM_ROLE_NAME}" >/dev/null 2>&1; then
  aws iam update-assume-role-policy --role-name "${SYSTEM_ROLE_NAME}" --policy-document "$SYSTEM_TRUST"
else
  aws iam create-role --role-name "${SYSTEM_ROLE_NAME}" --assume-role-policy-document "$SYSTEM_TRUST" >/dev/null
fi

if aws iam get-role --role-name "${TASK_ROLE_NAME}" >/dev/null 2>&1; then
  aws iam update-assume-role-policy --role-name "${TASK_ROLE_NAME}" --policy-document "$TASK_TRUST"
else
  aws iam create-role --role-name "${TASK_ROLE_NAME}" --assume-role-policy-document "$TASK_TRUST" >/dev/null
fi

SYSTEM_IAM_ROLE_ARN=$(aws iam get-role --role-name "${SYSTEM_ROLE_NAME}" --query 'Role.Arn' --output text)
TASK_IAM_ROLE_ARN=$(aws iam get-role --role-name "${TASK_ROLE_NAME}" --query 'Role.Arn' --output text)
{ echo "export SYSTEM_IAM_ROLE_ARN=${SYSTEM_IAM_ROLE_ARN}"
  echo "export TASK_IAM_ROLE_ARN=${TASK_IAM_ROLE_ARN}"; } >> "$UNION_ENV_FILE"
echo "SYSTEM_IAM_ROLE_ARN=${SYSTEM_IAM_ROLE_ARN}"; echo "TASK_IAM_ROLE_ARN=${TASK_IAM_ROLE_ARN}"
```

Unlike the docs' one-shot `create-role`, this updates the trust policy if the role already
exists, so re-running the step converges instead of failing with `EntityAlreadyExists`.

---

## Step 6 — Permissions on the roles

⛔ APPROVAL GATE. Attaches one inline policy to each role.

- **System policy** — full object access to the bucket, plus the Secrets Manager
  create/read/update operations the operator's secret proxy performs at runtime.
- **Task policy** — the same bucket access, read-only Secrets Manager, and
  `ecr:GetAuthorizationToken` so task pods can pull and push images.

`secretsmanager` resources are scoped to this account and region but wildcard the secret
name, because the data plane creates secrets on demand as users add them.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${METADATA_BUCKET:?}"; : "${AWS_REGION:?}"; : "${AWS_ACCOUNT_ID:?}"
: "${SYSTEM_ROLE_NAME:?}"; : "${TASK_ROLE_NAME:?}"

aws iam put-role-policy --role-name "${SYSTEM_ROLE_NAME}" --policy-name union-system-access \
  --policy-document "$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "UnionDataBuckets",
      "Effect": "Allow",
      "Action": ["s3:DeleteObject*", "s3:GetObject*", "s3:ListBucket", "s3:PutObject*"],
      "Resource": ["arn:aws:s3:::${METADATA_BUCKET}", "arn:aws:s3:::${METADATA_BUCKET}/*"]
    },
    {
      "Sid": "SecretsManagerReadWrite",
      "Effect": "Allow",
      "Action": [
        "secretsmanager:CreateSecret",
        "secretsmanager:DescribeSecret",
        "secretsmanager:GetSecretValue",
        "secretsmanager:PutSecretValue",
        "secretsmanager:UpdateSecret",
        "secretsmanager:TagResource"
      ],
      "Resource": "arn:aws:secretsmanager:${AWS_REGION}:${AWS_ACCOUNT_ID}:secret:*"
    }
  ]
}
EOF
)"

aws iam put-role-policy --role-name "${TASK_ROLE_NAME}" --policy-name union-task-access \
  --policy-document "$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "UnionDataBuckets",
      "Effect": "Allow",
      "Action": ["s3:DeleteObject*", "s3:GetObject*", "s3:ListBucket", "s3:PutObject*"],
      "Resource": ["arn:aws:s3:::${METADATA_BUCKET}", "arn:aws:s3:::${METADATA_BUCKET}/*"]
    },
    {
      "Sid": "SecretsManagerRead",
      "Effect": "Allow",
      "Action": ["secretsmanager:DescribeSecret", "secretsmanager:GetSecretValue"],
      "Resource": "arn:aws:secretsmanager:${AWS_REGION}:${AWS_ACCOUNT_ID}:secret:*"
    },
    {
      "Sid": "ECRTokenPermission",
      "Effect": "Allow",
      "Action": "ecr:GetAuthorizationToken",
      "Resource": "*"
    }
  ]
}
EOF
)"
```

🟢 READ-ONLY check — the bucket ARN must be a real name, not `arn:aws:s3:::`:

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
aws iam get-role-policy --role-name "$SYSTEM_ROLE_NAME" --policy-name union-system-access \
  --query 'PolicyDocument.Statement[?Sid==`UnionDataBuckets`].Resource' --output text
aws iam get-role-policy --role-name "$TASK_ROLE_NAME" --policy-name union-task-access \
  --query 'PolicyDocument.Statement[?Sid==`UnionDataBuckets`].Resource' --output text
```

> **Do not "tidy" that into a loop over `"$ROLE:union-system-access"` pairs split with
> `${r%%:*}` / `${r##*:}`.** In **zsh** — the default shell on macOS, and the one an agent's
> `Bash` tool usually gets — that collides with zsh's `:u` modifier: the role name comes back
> uppercased with a character eaten
> (`NIELS-TESTING-03-...-SYSTEMnion-system-access`) and the check dies with `NoSuchEntity`.
> The policies attached correctly; only the verification is broken, which makes it an
> especially confusing failure to read. Two explicit calls cost nothing.

`ecr:GetAuthorizationToken` cannot be scoped to a repository — AWS only accepts `"*"` for
it. The repository policy in step 7 is what actually limits which repository the token is
good for.

---

## Step 7 — The ECR repository policy

⛔ APPROVAL GATE. Grants the task role push and pull, and the system role plus the Auto Mode
**node** role pull-only.

The node role matters and is easy to overlook: the kubelet, not the pod's service account,
pulls the image. Leave it out and pods fail with `ImagePullBackOff` and a 403 from ECR even
though the task role's IRSA is perfect.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${ECR_REPO_NAME:?}"; : "${TASK_IAM_ROLE_ARN:?}"; : "${SYSTEM_IAM_ROLE_ARN:?}"; : "${NODE_ROLE_ARN:?}"

aws ecr set-repository-policy --repository-name "${ECR_REPO_NAME}" --region "${AWS_REGION}" \
  --policy-text "$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "TaskPushPull",
      "Effect": "Allow",
      "Principal": { "AWS": "${TASK_IAM_ROLE_ARN}" },
      "Action": [
        "ecr:BatchCheckLayerAvailability", "ecr:BatchGetImage", "ecr:CompleteLayerUpload",
        "ecr:DescribeImages", "ecr:DescribeRepositories", "ecr:GetDownloadUrlForLayer",
        "ecr:InitiateLayerUpload", "ecr:ListImages", "ecr:PutImage", "ecr:UploadLayerPart"
      ]
    },
    {
      "Sid": "SystemAndNodePull",
      "Effect": "Allow",
      "Principal": { "AWS": ["${SYSTEM_IAM_ROLE_ARN}", "${NODE_ROLE_ARN}"] },
      "Action": [
        "ecr:BatchCheckLayerAvailability", "ecr:BatchGetImage", "ecr:DescribeImages",
        "ecr:DescribeRepositories", "ecr:GetDownloadUrlForLayer", "ecr:ListImages"
      ]
    }
  ]
}
EOF
)"
```

IAM will reject a principal ARN that does not resolve, so a failure here usually means one
of the three ARNs is empty or stale — re-source the state file and check each one.

---

## Step 8 — Collect the values

🟢 READ-ONLY. This is the 📤 **TO THE UI** hand-off: print the six values, labelled exactly
as the Union.ai forms label them, and give this block to the human verbatim.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
: "${METADATA_BUCKET:?an earlier step did not complete}"
: "${AWS_ACCOUNT_ID:?an earlier step did not complete}"
: "${AWS_REGION:?an earlier step did not complete}"
: "${IMAGE_REGISTRY:?an earlier step did not complete}"
: "${SYSTEM_IAM_ROLE_ARN:?an earlier step did not complete}"
: "${TASK_IAM_ROLE_ARN:?an earlier step did not complete}"
: "${CLUSTER_NAME:?an earlier step did not complete}"

printf '%s\n' \
  "Cluster pool form:" \
  "  S3 Bucket           = s3://${METADATA_BUCKET}" \
  "  Account ID          = ${AWS_ACCOUNT_ID}" \
  "  Region              = ${AWS_REGION}" \
  "  Image registry      = ${IMAGE_REGISTRY}" \
  "" \
  "Connect cluster dialog:" \
  "  System IAM Role ARN = ${SYSTEM_IAM_ROLE_ARN}" \
  "  Task IAM Role ARN   = ${TASK_IAM_ROLE_ARN}" \
  "" \
  "kubeconfig: aws eks update-kubeconfig --region ${AWS_REGION} --name ${CLUSTER_NAME}"
```

### How to hand these over

The human is going to alt-tab between a terminal and a browser form. Make that as close to
mechanical as possible:

- **Print one field per line, labelled exactly as the form labels it.** Not prose, not a
  paragraph they have to mine for a value.
- **Give the two forms as two separate blocks**, in the order the UI asks for them — the four
  pool values, then the two role ARNs. Do not interleave them.
- **Hand over the pool values as soon as steps 3 and 4 finish**, without waiting for the
  cluster. See [the ordering section](#run-the-steps-in-this-order-not-in-numbered-order).
- **Say which values are live and which are not yet.** If the roles do not exist yet, mark
  them and say when they will.
- **Never bury a value in command output.** Re-print it in a clean block even if it scrolled
  past two commands ago; re-printing costs nothing and hunting causes mis-pastes.

Two details that cause silent mistakes:

- **The S3 value carries the `s3://` scheme.** The form wants `s3://my-bucket`, not
  `my-bucket`.
- **The image registry is the full repository URI**, including `/<repository-name>`. The
  registry *host* alone is the commonest wrong answer.

Keep the state file, and keep `kubectl` pointed at this cluster: `union-connect-cluster`
installs the agent from the same machine.

---

## Next

→ **`union-connect-cluster`.** Create the cluster pool with the four pool values, register
the cluster with the two role ARNs, and install the agent.

## Behaviour worth remembering

- **The wide service-account trust is intentional.** Narrow `DATAPLANE_NAMESPACE` only once
  the real `instance-<id>` release namespace is known and stable — see
  [references/iam-policies.md](references/iam-policies.md).
- **One bucket serves metadata and fast registration.** Do not add a second.
- **The data plane chart owns Metrics Server.** Never add the EKS Metrics Server add-on to
  this cluster.
- **Nothing here expires.** No S3 lifecycle rule, no ECR lifecycle policy. Add both before
  production, or the bucket and repository grow without bound.
- **Teardown** is not `eksctl delete cluster` alone — see
  [references/teardown.md](references/teardown.md) for the full list and the order.
