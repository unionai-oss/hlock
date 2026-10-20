# Teardown

Everything `union-provision-aws` creates, in the order it has to go. Read the whole page
before running any of it.

> **Every command here is destructive and most are irreversible.** ⛔ APPROVAL GATE —
> each one is its own. Before any of them, print the resource it targets and have the human
> confirm that specific name. A `NAME_PREFIX` that is one character off deletes someone
> else's cluster.

## Before anything

Disconnect the cluster in the Union.ai UI first. Deleting the EKS cluster out from under a
connected pool leaves Union.ai holding a registration for a cluster that no longer answers,
and the UI shows it as permanently unhealthy.

**Check for more than one registration.** A cluster that was connected, disconnected and
reconnected — or registered from two organizations — carries several data plane installs at
once, and *each* needs disconnecting in the UI. Count them from the cluster side:

```bash
# 🟢 READ-ONLY — one namespace per data plane install
kubectl get ns -o name | grep '^namespace/instance-'
kubectl get ns -l omnistrate.com/managed-by=omnistrate \
  -o custom-columns='NS:.metadata.name,HOST:.metadata.labels.omnistrate\.com/host-cluster-id' 2>/dev/null
helm -n dataplane-agent list      # REVISION well above 1 suggests repeated re-registration
```

Distinct `host-cluster-id` values are **distinct registrations** against this one EKS
cluster, not duplicates to ignore. The organization name is not recoverable from these
labels — `creator-org-name` is an opaque slug — so the human identifies them by finding the
cluster name in each organization's cluster list.

Because this is a UI step the agent cannot do or verify, **keep it on the visible blocked
list until the human confirms it**, and do not start step 2 on the assumption it happened.

Then confirm what you are about to destroy:

```bash
# 🟢 READ-ONLY
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
printf 'account = %s\nregion  = %s\ncluster = %s\nbucket  = %s\necr     = %s\nroles   = %s , %s\n' \
  "$AWS_ACCOUNT_ID" "$AWS_REGION" "$CLUSTER_NAME" "$METADATA_BUCKET" \
  "$ECR_REPO_NAME" "$SYSTEM_ROLE_NAME" "$TASK_ROLE_NAME"
aws sts get-caller-identity --query Account --output text   # must equal AWS_ACCOUNT_ID
```

If `get-caller-identity` disagrees with `AWS_ACCOUNT_ID`, stop. The shell is pointed at a
different account than the one the state file describes.

## 1. Check what the bucket holds

🟢 READ-ONLY. Workflow metadata, task inputs and outputs, and artifacts all live here. Once
it is emptied, every past run's data is gone and the UI's links to it break.

```bash
aws s3 ls "s3://${METADATA_BUCKET}" --recursive --summarize | tail -3
```

## 2. Delete the EKS cluster

⛔ APPROVAL GATE — 10–15 minutes. Removes the cluster, the Auto Mode nodes, the VPC, subnets, NAT gateways
and the node IAM role — everything in the `eksctl-${CLUSTER_NAME}-*` CloudFormation stacks.

```bash
eksctl delete cluster --name "${CLUSTER_NAME}" --region "${AWS_REGION}" --wait
```

If it fails, it is almost always a leftover load balancer or security group that an
in-cluster controller created and CloudFormation cannot delete:

```bash
# 🟢 READ-ONLY — find what is holding the VPC
aws cloudformation describe-stack-events --region "$AWS_REGION" \
  --stack-name "eksctl-${CLUSTER_NAME}-cluster" \
  --query 'StackEvents[?ResourceStatus==`DELETE_FAILED`].[LogicalResourceId,ResourceStatusReason]' \
  --output table
aws elbv2 describe-load-balancers --region "$AWS_REGION" \
  --query "LoadBalancers[?VpcId=='<vpc-id>'].[LoadBalancerName,LoadBalancerArn]" --output table
```

Delete those by hand, then re-run `eksctl delete cluster`.

## 3. Empty and delete the bucket

⛔ APPROVAL GATE — **irreversible.** `s3 rm --recursive` does not touch non-current versions or delete
markers, so on a versioned bucket the delete in the second command still fails with
`BucketNotEmpty`. The Python block below clears versions too.

```bash
aws s3 rm "s3://${METADATA_BUCKET}" --recursive

# Versioned buckets only — remove every version and delete marker.
aws s3api get-bucket-versioning --bucket "${METADATA_BUCKET}"
python3 - "$METADATA_BUCKET" <<'PY'
import subprocess, json, sys
bucket = sys.argv[1]
for key in ("Versions", "DeleteMarkers"):
    while True:
        out = subprocess.run(
            ["aws", "s3api", "list-object-versions", "--bucket", bucket,
             "--max-items", "500", "--query", f"{key}[].{{Key:Key,VersionId:VersionId}}",
             "--output", "json"],
            capture_output=True, text=True, check=True).stdout
        items = json.loads(out or "null") or []
        if not items:
            break
        subprocess.run(
            ["aws", "s3api", "delete-objects", "--bucket", bucket,
             "--delete", json.dumps({"Objects": items, "Quiet": True})], check=True)
        print(f"deleted {len(items)} {key}")
PY

aws s3api delete-bucket --bucket "${METADATA_BUCKET}" --region "${AWS_REGION}"
```

**The name is not reusable straight away.** S3 bucket names are global and a deleted name
can take a while to become available again — and someone else may take it. If you are
rebuilding, pick a new `BUCKET_PREFIX` rather than waiting.

## 4. Delete the ECR repository

⛔ APPROVAL GATE — `--force` deletes the images in it as well as the repository.

```bash
aws ecr describe-images --repository-name "${ECR_REPO_NAME}" --region "${AWS_REGION}" \
  --query 'length(imageDetails)' --output text      # 🟢 READ-ONLY — how many images you are about to lose
aws ecr delete-repository --repository-name "${ECR_REPO_NAME}" --region "${AWS_REGION}" --force
```

## 5. Delete the two IAM roles

⛔ APPROVAL GATE — inline policies must go first — IAM refuses to delete a role that still has any attached.

```bash
for role in "${SYSTEM_ROLE_NAME}" "${TASK_ROLE_NAME}"; do
  for p in $(aws iam list-role-policies --role-name "$role" --query 'PolicyNames[]' --output text); do
    aws iam delete-role-policy --role-name "$role" --policy-name "$p"
  done
  for p in $(aws iam list-attached-role-policies --role-name "$role" --query 'AttachedPolicies[].PolicyArn' --output text); do
    aws iam detach-role-policy --role-name "$role" --policy-arn "$p"
  done
  aws iam delete-role --role-name "$role"
done
```

## 6. Secrets Manager

Not created by `union-provision-aws`, but the **data plane creates secrets at runtime** in
this account and region as users add them. Deleting the cluster does not remove them, and
they keep billing.

```bash
# 🟢 READ-ONLY — review before deleting anything
aws secretsmanager list-secrets --region "$AWS_REGION" \
  --query 'SecretList[].{name: Name, created: CreatedDate}' --output table
```

Delete only the ones you recognise as Union.ai's. `delete-secret` has a recovery window
(7–30 days) by default; `--force-delete-without-recovery` skips it and is irreversible.

## 7. The OIDC provider

`eksctl delete cluster` normally removes it. Check for an orphan:

```bash
aws iam list-open-id-connect-providers --query 'OpenIDConnectProviderList[].Arn' --output text | tr '\t' '\n' | grep "${CLUSTER_NAME}" || echo "(none left)"
```

An OIDC provider costs nothing, so leave it unless you are cleaning up thoroughly.

## 8. The state file

```bash
shred -u "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}" 2>/dev/null \
  || rm -f "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
```

## Confirm it is gone

```bash
# 🟢 READ-ONLY — "not found" on every line is the goal
aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" 2>&1 | tail -1
aws s3api head-bucket --bucket "$METADATA_BUCKET" 2>&1 | tail -1
aws ecr describe-repositories --region "$AWS_REGION" --repository-names "$ECR_REPO_NAME" 2>&1 | tail -1
aws iam get-role --role-name "$SYSTEM_ROLE_NAME" 2>&1 | tail -1
aws iam get-role --role-name "$TASK_ROLE_NAME" 2>&1 | tail -1
aws cloudformation describe-stacks --region "$AWS_REGION" \
  --query "Stacks[?starts_with(StackName, 'eksctl-${CLUSTER_NAME}')].StackName" --output text
```

Then check Cost Explorer a day later. EBS volumes, Elastic IPs and load balancers created by
in-cluster controllers outlive their cluster more often than anything else.

## What this does not touch

- The **AWS Marketplace subscription** — cancel it in the Marketplace console if you are
  done with Union.ai. It bills independently of any AWS resource.
- The **Union.ai organization** — contact Union.ai to remove it.
- Anything in an account other than `AWS_ACCOUNT_ID`.
