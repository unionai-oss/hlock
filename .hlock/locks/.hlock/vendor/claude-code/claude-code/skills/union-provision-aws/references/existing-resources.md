# Using AWS resources you already have

`union-provision-aws` is optional. An existing EKS cluster, S3 bucket, ECR repository and
IAM roles work as a self-serve cluster pool if they meet the requirements below.

Work through this as a checklist. Each row has the command that answers it, the answer that
passes, and what breaks if it does not. All of these are 🟢 READ-ONLY — nothing here changes
anything.

Set the names first:

```bash
export AWS_REGION="<region>" CLUSTER_NAME="<cluster>"
export METADATA_BUCKET="<bucket>" ECR_REPO_NAME="<repo>"
export SYSTEM_ROLE_NAME="<system-role>" TASK_ROLE_NAME="<task-role>"
export AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
```

---

## The EKS cluster

### It has an IAM OIDC provider

```bash
aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
  --query 'cluster.identity.oidc.issuer' --output text
aws iam list-open-id-connect-providers
```

The issuer URL must be non-empty **and** its host must appear in the provider list. An
issuer exists on every EKS cluster; the *provider* is a separate IAM object you have to
associate. Without it, IRSA does not work at all and every pod falls back to the node role.

Fix: `eksctl utils associate-iam-oidc-provider --cluster "$CLUSTER_NAME" --region "$AWS_REGION" --approve`.

### It can add nodes on demand

```bash
aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
  --query '{automode: cluster.computeConfig.enabled, noderole: cluster.computeConfig.nodeRoleArn}'
kubectl get nodes
kubectl -n kube-system get deploy 2>/dev/null | grep -Ei 'karpenter|cluster-autoscaler' || echo "(no autoscaler in kube-system)"
kubectl get deploy -A | grep -Ei 'karpenter' || true
```

Either Auto Mode is enabled, or a cluster autoscaler / Karpenter is running, or the node
group is large enough to hold the data plane plus your workloads with headroom. The data
plane is several pods and will not fit on an idle two-node `t3.small` group.

If Auto Mode is **off**, `cluster.computeConfig.nodeRoleArn` is null. That is fine for a
managed node group — but then use the node group's instance role wherever
`union-provision-aws` uses `NODE_ROLE_ARN` (the ECR repository policy in step 7):

```bash
aws eks describe-nodegroup --region "$AWS_REGION" --cluster-name "$CLUSTER_NAME" \
  --nodegroup-name "<nodegroup>" --query 'nodegroup.nodeRole' --output text
```

### It has no Metrics Server of its own

```bash
kubectl get clusterrole system:metrics-server-aggregated-reader >/dev/null 2>&1 \
  && echo "CONFLICT: Metrics Server present" || echo "OK"
kubectl get apiservice v1beta1.metrics.k8s.io 2>/dev/null
aws eks list-addons --region "$AWS_REGION" --cluster-name "$CLUSTER_NAME" --output text | grep -i metrics || true
```

**The data plane chart installs Metrics Server.** A second one — whether the EKS add-on, a
Helm release, or something a platform team installed — collides on the same cluster-scoped
objects and the data plane install fails or flaps.

Fix: remove the existing one before connecting the cluster. `aws eks delete-addon
--addon-name metrics-server ...` for the add-on, or `helm uninstall` for a chart. If
something else on the cluster genuinely depends on it, this cluster is not a good candidate
for self-serve; use a different one.

---

## The S3 bucket

### Public access is blocked

```bash
aws s3api get-public-access-block --bucket "$METADATA_BUCKET"
```

All four flags true.

### It has the CORS rule

```bash
aws s3api get-bucket-cors --bucket "$METADATA_BUCKET"
```

Must allow `GET, PUT, POST, DELETE, HEAD` from `https://*.unionai.cloud` and
`https://*.union.ai`, expose `ETag`, and allow all headers. `NoSuchCORSConfiguration` means
it is missing — apply the rule from step 3 of the skill.

Symptom if it is wrong: runs execute normally, but the UI cannot show inputs, outputs, or
code, and the browser console reports blocked cross-origin requests. Nothing in the cluster
looks broken.

### One bucket, not two

Union.ai uses this single bucket for metadata *and* fast registration. If you have a
separate fast-registration bucket from an older deployment, it is not used here — there is
no field for it.

---

## The ECR repository

```bash
aws ecr describe-repositories --region "$AWS_REGION" --repository-names "$ECR_REPO_NAME" \
  --query 'repositories[0].{uri: repositoryUri, visibility: repositoryUri}' --output json
aws ecr get-repository-policy --region "$AWS_REGION" --repository-name "$ECR_REPO_NAME" \
  --query 'policyText' --output text | jq .
```

- **Private**, not public. A public ECR repository lives under a different service
  (`ecr-public`) and has a different URI shape.
- The policy must grant the **task role** push and pull, and the **system role** and the
  **node role** pull. See step 7 of the skill for the exact document.

The node role is the one people forget. The kubelet pulls images using the node's instance
role, not the pod's service account, so omitting it produces `ImagePullBackOff` with a 403
even when the task role is configured perfectly.

---

## The two IAM roles

### Trust policies

```bash
aws iam get-role --role-name "$SYSTEM_ROLE_NAME" --query 'Role.AssumeRolePolicyDocument' | jq .
aws iam get-role --role-name "$TASK_ROLE_NAME" --query 'Role.AssumeRolePolicyDocument' | jq .
```

Both must federate to **this cluster's** OIDC provider ARN
(`arn:aws:iam::<account>:oidc-provider/oidc.eks.<region>.amazonaws.com/id/<id>`), condition
`aud == sts.amazonaws.com`, and match these subjects:

| Role | `sub` must match |
|---|---|
| System | `system:serviceaccount:<ns>:union-system` and `system:serviceaccount:<ns>:flytepropeller-system` |
| Task | `system:serviceaccount:*:default` and `system:serviceaccount:*:union` |

where `<ns>` is `*` unless you already know the `instance-<id>` release namespace.

A role reused from a *different* cluster will have a different OIDC provider in its trust
policy and will silently refuse every `AssumeRoleWithWebIdentity`. This is the single most
common failure when reusing roles — check the provider ID, not just the shape.

### Permissions

```bash
aws iam list-role-policies --role-name "$SYSTEM_ROLE_NAME"
aws iam list-attached-role-policies --role-name "$SYSTEM_ROLE_NAME"
aws iam get-role-policy --role-name "$SYSTEM_ROLE_NAME" --policy-name "<name>" | jq .PolicyDocument
```

| Role | Needs |
|---|---|
| System | `s3:ListBucket` on the bucket; `s3:GetObject*`/`PutObject*`/`DeleteObject*` on `bucket/*`; Secrets Manager `CreateSecret`, `DescribeSecret`, `GetSecretValue`, `PutSecretValue`, `UpdateSecret`, `TagResource` in this account and region |
| Task | the same S3 access; Secrets Manager `DescribeSecret` + `GetSecretValue`; `ecr:GetAuthorizationToken` on `*` |

Attached managed policies count as well as inline ones — a role with `AmazonS3FullAccess`
attached satisfies the S3 half, though it grants far more than needed.

Verify rather than eyeball, using the IAM policy simulator:

```bash
aws iam simulate-principal-policy \
  --policy-source-arn "arn:aws:iam::${AWS_ACCOUNT_ID}:role/${TASK_ROLE_NAME}" \
  --action-names s3:GetObject s3:PutObject s3:ListBucket secretsmanager:GetSecretValue \
  --resource-arns "arn:aws:s3:::${METADATA_BUCKET}/x" \
  --query 'EvaluationResults[].{action: EvalActionName, decision: EvalDecision}' --output table
```

`ListBucket` evaluates against the bucket ARN rather than an object ARN, so run it
separately with `--resource-arns "arn:aws:s3:::${METADATA_BUCKET}"` if it shows as denied.

---

## Then

Collect the six values and continue with `union-connect-cluster`:

```bash
printf '%s\n' \
  "  S3 Bucket           = s3://${METADATA_BUCKET}" \
  "  Account ID          = ${AWS_ACCOUNT_ID}" \
  "  Region              = ${AWS_REGION}" \
  "  Image registry      = $(aws ecr describe-repositories --region "$AWS_REGION" \
        --repository-names "$ECR_REPO_NAME" --query 'repositories[0].repositoryUri' --output text)" \
  "  System IAM Role ARN = $(aws iam get-role --role-name "$SYSTEM_ROLE_NAME" --query Role.Arn --output text)" \
  "  Task IAM Role ARN   = $(aws iam get-role --role-name "$TASK_ROLE_NAME" --query Role.Arn --output text)"
```

Write them into the state file too, so `union-connect-cluster` and `union-debug-cluster` can
read them:

```bash
UNION_ENV_FILE="${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
touch "$UNION_ENV_FILE" && chmod 600 "$UNION_ENV_FILE"
{
  echo "export AWS_REGION=${AWS_REGION}"
  echo "export AWS_ACCOUNT_ID=${AWS_ACCOUNT_ID}"
  echo "export CLUSTER_NAME=${CLUSTER_NAME}"
  echo "export METADATA_BUCKET=${METADATA_BUCKET}"
  echo "export ECR_REPO_NAME=${ECR_REPO_NAME}"
  echo "export SYSTEM_ROLE_NAME=${SYSTEM_ROLE_NAME}"
  echo "export TASK_ROLE_NAME=${TASK_ROLE_NAME}"
} >> "$UNION_ENV_FILE"
```
