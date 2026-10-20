# The IAM model, and how to narrow it

Self-serve setup creates two IRSA roles with deliberately wide service-account trust. This
explains what that grants, why, and what you can safely tighten once the cluster is running.

## Why IRSA at all

EKS pods get AWS credentials one of two ways: the node's instance role (every pod on the
node shares it) or **IRSA** — IAM Roles for Service Accounts, where the pod's projected
service-account token is exchanged for role credentials via
`sts:AssumeRoleWithWebIdentity`. IRSA is what lets the data plane and task pods hold
*different* permissions on the same node, which is the whole point of splitting the system
and task roles.

The exchange only works if the cluster has an **IAM OIDC provider** registered for its
issuer. The issuer exists on every EKS cluster; the provider is a separate IAM object, which
is why `union-provision-aws` runs `eksctl utils associate-iam-oidc-provider` as its own step.

## The two roles

| | System role | Task role |
|---|---|---|
| Assumed by | `union-system`, `flytepropeller-system` | `default`, `union` |
| In namespace | the data plane release namespace | every project namespace |
| S3 | read/write/delete on the bucket | same |
| Secrets Manager | create, read, update, tag | describe + read only |
| ECR | pull (via the repository policy) | `GetAuthorizationToken`, push and pull |

The split is meaningful: **task pods run user code.** They can read secrets but not create
or overwrite them, so a task cannot rewrite a credential another team depends on.

`flytepropeller-system` is the legacy name of the system service account and is included
for compatibility with data plane versions that still use it. Leaving it out will appear to
work until you hit a version that does not, so keep it.

## The wildcards, and what they cost

### `DATAPLANE_NAMESPACE='*'` in the system trust policy

The release namespace is `instance-<identifier>`, and the identifier is chosen by Union.ai
when it installs the data plane through the agent — **it does not exist at the time you
create the role**. So the trust policy has to match any namespace.

What that grants: a service account named exactly `union-system` or
`flytepropeller-system`, **in any namespace of this cluster**, can assume the system role.
Anyone who can create a namespace and a service account with that name on this cluster can
therefore reach the bucket and Secrets Manager. On a cluster whose namespace-creation rights
are limited to platform administrators, that is a small exposure. On a shared,
multi-tenant cluster it is not.

### `system:serviceaccount:*:default` in the task trust policy

Project namespaces are created on demand by the data plane, so this one cannot be narrowed
by namespace at all without re-editing the trust policy each time a project is added. Note
that it matches the `default` service account, which **every** namespace has — so any pod on
the cluster that does not explicitly set a service account can assume the task role.

This is the wider of the two. If it is unacceptable for your cluster, the mitigation is a
dedicated cluster for Union.ai workloads rather than a narrower policy.

## Narrowing after the install

Once the data plane is installed, the release namespace is fixed. Find it:

```bash
kubectl get ns -o name | grep '^namespace/instance-'
```

Then replace `*` with that namespace in the **system** role's trust policy only:

```bash
source "${UNION_ENV_FILE:-$PWD/.union-selfserve.env}"
DATAPLANE_NAMESPACE=$(kubectl get ns -o name | grep '^namespace/instance-' | head -1 | cut -d/ -f2)
: "${DATAPLANE_NAMESPACE:?data plane namespace not found — is the install complete?}"

aws iam update-assume-role-policy --role-name "${SYSTEM_ROLE_NAME}" --policy-document "$(cat <<EOF
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
)"
```

⛔ APPROVAL GATE — a wrong namespace here locks the data plane out of S3 and
Secrets Manager, and the failure shows up as tasks failing to write outputs rather than as
an IAM error.

**Do not narrow the task role's namespace wildcard** — project namespaces keep being
created.

**The namespace can change.** A reinstall or a cluster re-registration produces a new
`instance-<id>`. If you have narrowed the system role, add "re-check the trust policy" to
your reinstall runbook, or the data plane comes back up unable to reach S3.

## Secrets Manager scope

Both policies scope to `arn:aws:secretsmanager:<region>:<account>:secret:*` — this account
and region, any secret name. Names cannot be predicted because users create secrets through
Union.ai at runtime.

To bound it, have the data plane create secrets under a known prefix and scope to
`secret:union/*`. Confirm the prefix the installed version actually uses before relying on
it — get it wrong and secret creation fails at the moment a user first needs one.

## S3 scope

Both roles get `ListBucket` on the bucket ARN and object actions on `bucket/*`. The two ARN
forms are both required: `s3:ListBucket` is authorized against the *bucket*, every object
action against the *object*. A policy with only `arn:aws:s3:::bucket/*` produces "access
denied" on listing while individual object reads work — a confusing half-failure.

`s3:GetObject*` and `s3:PutObject*` use the wildcard suffix on purpose: it covers
`GetObjectVersion`, `PutObjectAcl` and the tagging variants the SDK uses for multipart
uploads.

## What is not here

- **KMS.** The bucket uses SSE-S3. Switching to SSE-KMS needs `kms:Decrypt`,
  `kms:GenerateDataKey` and friends added to both roles, and a key policy allowing them.
- **VPC endpoints.** Traffic to S3, ECR, STS and Secrets Manager goes out through the NAT
  gateway. Gateway and interface endpoints cut that cost and keep the traffic off the public
  internet.
- **Permissions boundaries** and **`aws:RequestedRegion`/`aws:SourceVpc`** conditions, both
  of which meaningfully reduce the wildcard exposure above if your organization uses them.
