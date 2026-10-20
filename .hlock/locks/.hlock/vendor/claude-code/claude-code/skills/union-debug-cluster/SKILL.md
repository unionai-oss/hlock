---
name: union-debug-cluster
description: 'Diagnose a Union.ai self-serve cluster that is unhealthy, stuck installing, or failing at runtime — agent not connecting, the data plane never appearing, IRSA trust mismatches, S3 or ECR access denied, Auto Mode not scheduling, Metrics Server collisions, and missing bucket CORS. Use for "cluster shows unhealthy", "dp-agent CrashLoopBackOff", "Union data plane stuck", "instance- namespace", "ImagePullBackOff", or "tasks fail to write outputs".'
---

# Debugging a Union.ai self-serve cluster

Every check here is 🟢 **READ-ONLY** unless explicitly marked as an ⛔ APPROVAL GATE.
Diagnose completely before changing anything: most of these failures look alike from the
UI, and the most expensive mistake is reinstalling the agent when the real problem is an
IAM trust policy — the reinstall then produces a *new* `instance-<id>` namespace and a
fresh set of symptoms layered on the old ones.

---

## 0. First: is it actually broken?

Two states look like failure and are not.

**Unhealthy right after connecting is expected.** The cluster page shows **Unhealthy** for
the whole second install phase, while Union.ai pushes the data plane down the agent's
connection. On a new Auto Mode cluster that takes several minutes — pulling images, and
launching EC2 instances for the first time because the cluster had no nodes at all. Under
~15 minutes since the agent connected: wait and re-check.

**No nodes on an idle Auto Mode cluster is correct.** Auto Mode starts nodes when there are
pods to run. `kubectl get nodes` returning nothing before the data plane exists is the
designed behaviour, not a missing node group.

So establish the timeline first: *when did the agent install run, and what does the
Installation progress panel say now?* Then run the snapshot.

## 1. Take a snapshot

```bash
bash scripts/collect.sh -o /tmp/union-health.txt
```

`scripts/collect.sh` is relative to this skill's own directory. If that path does not
resolve, find it rather than giving up — where it lands depends on the harness:

```bash
find ~/.claude/skills ~/.agents/skills ~/.hermes/skills ~/.config/opencode/skills \
     ~/.pi/agent/skills ./.claude/skills ./.agents/skills \
     -name collect.sh -path '*union-debug-cluster*' 2>/dev/null | head -1
```

**If it reports the API server unreachable, read the message before assuming the cluster is
broken.** `Token has expired and refresh failed`, `ExpiredToken`, or
`exec: executable aws failed with exit code 255` all mean the *local* AWS credentials have
lapsed, not that anything is wrong in the cluster. Re-authenticate (`aws sso login`, or
refresh whatever issues the credentials) and run it again. The kubeconfig entry shells out
to `aws` on every call, so an expired session looks exactly like an unreachable cluster.

Read-only throughout. It checks cluster access, nodes, the agent namespace, the data plane
namespace, Metrics Server collisions, unhealthy pods, warning events, and — if `aws` is
available and the state file has the names — the bucket, the OIDC provider, the two IAM
roles' trust policies, and the ECR repository policy. Output is redacted for tokens, keys,
presigned URLs and account IDs, but read it before pasting anywhere.

It sources `${UNION_ENV_FILE:-$PWD/.union-selfserve.env}`. Without that file the AWS half is
skipped rather than failed, and the kubectl half still runs.

Then use the summary line to enter the table below.

## 2. Symptom → cause

| Symptom | Most likely cause | Confirm with | §|
|---|---|---|---|
| No `dataplane-agent` namespace | The install command never ran, or ran against a different cluster | `kubectl config current-context` | [3](#3-the-agent) |
| Agent pod `CrashLoopBackOff` | Bad or truncated `values.yaml`, or egress blocked | agent logs | [3](#3-the-agent) |
| Agent `Running`, UI still says not connected | Outbound HTTPS blocked by a proxy, NACL or SG | agent logs, egress test | [3](#3-the-agent) |
| Agent connected, no `instance-*` namespace after 15 min | Union.ai side, or the agent lost its connection | agent logs, the UI panel | [4](#4-the-data-plane) |
| Data plane pods `Pending` | Auto Mode has not scheduled — quota, subnet IPs, or taints | `describe pod` events | [5](#5-scheduling-and-auto-mode) |
| Data plane pods `ImagePullBackOff` | ECR repository policy missing the **node** role | `describe pod` events | [7](#7-ecr) |
| Data plane pods `CrashLoopBackOff` with AWS errors | IRSA trust policy does not match | pod logs, trust policy | [6](#6-irsa-and-iam) |
| Install fails on Metrics Server / apiservice conflicts | A second Metrics Server on the cluster | `kubectl get deploy -A` | [8](#8-metrics-server) |
| Cluster Healthy, but tasks fail writing outputs | S3 permissions, or the wrong bucket in the pool | task pod logs | [6](#6-irsa-and-iam) |
| Cluster Healthy, UI cannot show inputs/outputs/code | Bucket CORS rule missing | `get-bucket-cors` | [9](#9-the-ui-cannot-show-data) |
| `flyte run` fails, cluster looks fine | CLI config, project/domain, or endpoint | `flyte get project` | [10](#10-the-client-side) |

---

## 3. The agent

The agent is the `dp-agent` Helm release in namespace `dataplane-agent`. It is the only
thing *you* install; everything else arrives through it.

```bash
kubectl -n dataplane-agent get pods -o wide
helm -n dataplane-agent list
kubectl -n dataplane-agent logs -l app.kubernetes.io/name=dp-agent --tail=100 --all-containers
kubectl -n dataplane-agent describe pod -l app.kubernetes.io/name=dp-agent | sed -n '/^Events:/,$p'
```

> ⚠️ Agent logs can contain tokens. Pipe through the redaction in `scripts/collect.sh`
> before sharing, and never print the `dp-agent` Secret or the `values.yaml`.

**Namespace missing entirely.** The install command did not run, or ran against a different
cluster. Check `kubectl config current-context` matches the cluster registered in the UI —
a laptop with several kube contexts is the usual explanation. Re-run the install from a
shell whose context is right (see `union-connect-cluster` §3).

**Pod `CrashLoopBackOff` immediately.** Almost always the values block: a truncated **Copy
install command** paste, or two copies' blocks mixed together. Do not try to repair the
file. Have the human copy the command again from the cluster page and re-run it — it is
`helm upgrade --install`, so re-running is safe.

**Pod `Running` but the UI never shows it connected.** The agent dials *out* over HTTPS. If
that is blocked, the pod stays up and retries. Test egress from inside the cluster:

```bash
# -i without -t: an agent's shell has no TTY, and `-it` fails there.
kubectl -n dataplane-agent run egress-test --rm -i --restart=Never \
  --image=public.ecr.aws/docker/library/curl:latest -- \
  -sS -m 20 -o /dev/null -w '%{http_code}\n' https://www.union.ai/
```

(That pod is transient and self-deleting, but it *is* a create — ⛔ APPROVAL GATE, ask first.) A hang or
connection error means egress is blocked: check the NAT gateway and route tables for the
node subnets, security group egress rules, any NACL, and any corporate proxy. Private
subnets with no NAT gateway are the most common version of this — the cluster comes up, the
agent starts, and nothing can reach the internet.

**Helm timed out but pods look fine.** `--wait --timeout 10m0s` can expire while an Auto
Mode cluster is still launching its first node. Check the pods before reinstalling:

```bash
kubectl -n dataplane-agent get pods
helm -n dataplane-agent status dp-agent --output json | jq -r '.info.status, .info.description'
```

If the pods are `Running`, the install worked and the timeout was cosmetic.

## 4. The data plane

Union.ai installs the data plane into a namespace named `instance-` plus an identifier. It
does not exist until Union.ai creates it, which is why you must list pods across **all**
namespaces.

```bash
DP_NS=$(kubectl get ns -o name | grep '^namespace/instance-' | head -1 | cut -d/ -f2)
echo "data plane namespace: ${DP_NS:-<none yet>}"
kubectl -n "${DP_NS:-default}" get pods -o wide
kubectl -n "${DP_NS:-default}" get events --sort-by=.lastTimestamp | tail -30
```

No namespace after 15 minutes with the agent connected and healthy: the install is stalled
on the Union.ai side, or the agent's connection dropped after registering. Check the agent
logs for reconnect loops, then the Installation progress panel — if it is still on phase one
the agent never actually connected, which is §3.

More than one `instance-*` namespace means the cluster was disconnected and re-registered.
Only the current one is live; the old one is an orphan whose pods will fail on revoked
credentials. Confirm which is current in the UI before deleting anything.

## 5. Scheduling and Auto Mode

```bash
kubectl -n "$DP_NS" describe pod "<pending-pod>" | sed -n '/^Events:/,$p'
kubectl get nodes -o wide
kubectl get events -A --field-selector reason=FailedScheduling --sort-by=.lastTimestamp | tail -20
```

What the events say, and what it means:

- **`Insufficient cpu/memory`, no new node appearing** — Auto Mode is not launching. Check
  the EC2 service quota for the instance family in this region (`vCPU limit` on a new
  account is often too low), and whether the subnets have free IP addresses.
- **`0/N nodes are available: N node(s) had untainted...`** — node pools do not match. Auto
  Mode was configured with `general-purpose` and `system`; confirm both exist:

```bash
aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
  --query 'cluster.computeConfig' --output json
```

`enabled: false` means the cluster is not in Auto Mode at all — it was created without
`autoModeConfig`, and needs either a managed node group or a cluster autoscaler.

- **Pending with no events** — usually a missing CSI driver for a PVC. `kubectl get pvc -A`.

```bash
# Is there simply no room?
kubectl top nodes 2>/dev/null || echo "(metrics unavailable — see §8)"
kubectl describe nodes | grep -A6 'Allocated resources'
```

## 6. IRSA and IAM

This is the subtlest failure: pods start, then fail on AWS calls. The message is usually
`AccessDenied`, `NoCredentialProviders`, or a timeout reaching STS.

**Check the annotation and the trust policy agree.** The service account must carry the role
ARN, and the role must trust *this* cluster's OIDC provider for *that* service account name
and namespace.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
DP_NS=$(kubectl get ns -o name | grep '^namespace/instance-' | head -1 | cut -d/ -f2)

kubectl -n "$DP_NS" get sa -o custom-columns=\
'NAME:.metadata.name,ROLE:.metadata.annotations.eks\.amazonaws\.com/role-arn'

OIDC=$(aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
  --query 'cluster.identity.oidc.issuer' --output text | sed 's|https://||')
echo "cluster OIDC: $OIDC"
aws iam get-role --role-name "$SYSTEM_ROLE_NAME" --query 'Role.AssumeRolePolicyDocument' | jq .
aws iam get-role --role-name "$TASK_ROLE_NAME"   --query 'Role.AssumeRolePolicyDocument' | jq .
```

Three things must line up, and a mismatch in any one produces the same silent denial:

1. **The OIDC provider in the trust policy is this cluster's.** A role reused from another
   cluster — or from a cluster that was deleted and recreated, which gets a *new* issuer ID —
   fails here. Compare the `id/<...>` segment character by character.
2. **The IAM OIDC provider object exists.** The issuer URL is present on every EKS cluster;
   the IAM provider is separate.

```bash
aws iam list-open-id-connect-providers --output text | grep "${OIDC##*/}" \
  || echo "MISSING — run: eksctl utils associate-iam-oidc-provider --cluster $CLUSTER_NAME --region $AWS_REGION --approve"
```

3. **The `sub` condition matches the real namespace and service account.** `union-system`
   and `flytepropeller-system` in the data plane namespace; `default` and `union` in project
   namespaces. If `DATAPLANE_NAMESPACE` was narrowed to a specific `instance-<id>` and the
   cluster has since been re-registered, the namespace changed and the trust policy no longer
   matches — see
   [iam-policies.md](../union-provision-aws/references/iam-policies.md#narrowing-after-the-install).

**Verify from inside a pod.** This is the definitive test — it exercises the real token
exchange rather than reading policies:

```bash
kubectl -n "$DP_NS" exec "deploy/<a-data-plane-deployment>" -- sh -c \
  'echo "role: $AWS_ROLE_ARN"; echo "token: $AWS_WEB_IDENTITY_TOKEN_FILE"; ls -l $AWS_WEB_IDENTITY_TOKEN_FILE'
```

Empty `AWS_ROLE_ARN` means the pod was admitted before the service account was annotated —
the webhook injects those variables at pod creation. Restart the deployment (⛔ APPROVAL GATE) and they
appear.

**Check permissions, not just trust:**

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
aws iam simulate-principal-policy \
  --policy-source-arn "$SYSTEM_IAM_ROLE_ARN" \
  --action-names s3:PutObject s3:GetObject secretsmanager:GetSecretValue \
  --resource-arns "arn:aws:s3:::${METADATA_BUCKET}/x" \
  --query 'EvaluationResults[].{action:EvalActionName,decision:EvalDecision}' --output table

aws iam simulate-principal-policy --policy-source-arn "$SYSTEM_IAM_ROLE_ARN" \
  --action-names s3:ListBucket --resource-arns "arn:aws:s3:::${METADATA_BUCKET}" \
  --query 'EvaluationResults[].{action:EvalActionName,decision:EvalDecision}' --output table
```

`ListBucket` is authorized against the **bucket** ARN and every object action against the
**object** ARN. A policy with only `arn:aws:s3:::bucket/*` denies listing while individual
reads succeed — which presents as intermittent, partial breakage rather than a clean denial.

## 7. ECR

`ImagePullBackOff` on data plane or task pods:

```bash
kubectl -n "$DP_NS" describe pod "<pod>" | sed -n '/^Events:/,$p' | tail -20
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
aws ecr get-repository-policy --region "$AWS_REGION" --repository-name "$ECR_REPO_NAME" \
  --query policyText --output text | jq .
aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
  --query 'cluster.computeConfig.nodeRoleArn' --output text
```

**The node role must be in the `SystemAndNodePull` principals.** The kubelet pulls images
using the *node's* instance role, not the pod's service account — so a perfect task-role
IRSA setup still yields a 403 if the node role is missing from the repository policy. This
is the single most common ECR failure here. Fix: re-run step 7 of `union-provision-aws` with
the correct `NODE_ROLE_ARN` (⛔ APPROVAL GATE).

Pulling from a *different* registry (Docker Hub, public ECR, GHCR) that rate-limits or
requires auth is a separate problem with the same symptom — read the actual event message
before assuming it is the repository policy.

## 8. Metrics Server

**The data plane chart installs Metrics Server.** A second one collides on cluster-scoped
resources and breaks the install or leaves it flapping.

```bash
kubectl get deploy -A | grep -i metrics-server
kubectl get apiservice v1beta1.metrics.k8s.io -o yaml 2>/dev/null | grep -E 'service:|name:|namespace:'
kubectl get clusterrole system:metrics-server-aggregated-reader -o jsonpath='{.metadata.annotations}' 2>/dev/null
aws eks list-addons --region "$AWS_REGION" --cluster-name "$CLUSTER_NAME" --output text | grep -i metrics
```

Exactly one deployment is correct, and on a self-serve cluster it should be the data plane's.
If the EKS add-on is installed, remove it (⛔ APPROVAL GATE — it will briefly break `kubectl top`):

```bash
aws eks delete-addon --region "$AWS_REGION" --cluster-name "$CLUSTER_NAME" --addon-name metrics-server
```

If a Helm-installed one is there instead, `helm uninstall` it. Then let the data plane
install settle, or reinstall the agent.

Prevention: the cluster config in `union-provision-aws` sets `disableDefaultAddons: true`
precisely for this. Never add the EKS Metrics Server add-on to a self-serve cluster.

## 9. The UI cannot show data

Runs execute fine, but the UI shows nothing for inputs, outputs, code or artifacts, and the
browser console reports blocked cross-origin requests.

That is the **bucket CORS rule**, not a cluster problem — the browser fetches those objects
directly from S3 through presigned URLs.

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
aws s3api get-bucket-cors --bucket "$METADATA_BUCKET" | jq .
```

`NoSuchCORSConfiguration`, or `AllowedOrigins` missing `https://*.unionai.cloud`, is the
answer. Re-apply the rule from `union-provision-aws` step 3 (⛔ APPROVAL GATE). A UI served from a
non-Union.ai domain needs its hostname added.

Also confirm the pool points at the bucket you are inspecting — a pool created with a typo'd
bucket name produces exactly this, and the cluster stays Healthy throughout.

## 10. The client side

Cluster Healthy but `flyte run` fails:

```bash
cat .flyte/config.yaml 2>/dev/null || echo "no config in $PWD"
flyte get project
flyte --version
```

- **Config is per-directory.** `flyte run` reads `.flyte/config.yaml` from the current
  directory or an ancestor. Running from elsewhere gives an auth or endpoint error rather
  than a missing-config error.
- **Endpoint shape.** Bare hostname: `myorg.hosted.unionai.cloud`. No scheme, no trailing
  slash.
- **`--tracked` runs never touch the cluster.** They execute on the local machine and land
  under **Tracked Runs**. If a user reports "it ran but not on my cluster", check for the
  flag first.
- **First cluster run is slow.** The image is built and pushed to ECR, and Auto Mode may be
  launching a node. Minutes, not seconds.
- **Stale auth** after an org or identity change: re-run `flyte create config` and sign in
  again.

---

## Escalating

Before contacting Union.ai support, gather:

```bash
bash scripts/collect.sh -o /tmp/union-health.txt
```

plus: the organization name, the cluster name as registered in Union.ai, what the
Installation progress panel shows, and when the agent install ran. **Review the file before
sending it** — the redaction covers JWTs, AWS keys, presigned URL parameters, private-key
headers and 12-digit account IDs, but it cannot anticipate everything, and pod logs are the
least predictable part.

Never send `values.yaml`, the `dp-agent` Kubernetes Secret, or the install command — they
contain the agent's private key. If one has been exposed, disconnect the cluster in the UI
and re-register it; that issues a new credential.

## Starting over

⛔ APPROVAL GATE — each of these is destructive.

**Reinstall the agent** — safe, idempotent, and the right move for a corrupted values block.
Copy the install command fresh from the cluster page and re-run it (`helm upgrade
--install`).

**Disconnect and re-register the cluster** — when the registration itself is wrong (bad role
ARNs, wrong pool). This invalidates the old credential, creates a new `instance-<id>`
namespace, and requires a new cluster name. Re-check any narrowed IAM trust policy
afterwards.

**Delete everything and start again** — see
[teardown.md](../union-provision-aws/references/teardown.md). Read it fully first: the
bucket holds every past run's data, and emptying it is irreversible.
