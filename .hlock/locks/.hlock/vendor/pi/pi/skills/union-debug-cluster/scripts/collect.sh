#!/usr/bin/env bash
# Collect a health snapshot of a Union.ai self-serve data plane.
#
# Read-only: every command is a get/describe/list. It creates no resources,
# changes no configuration, and needs no approval gate.
#
#   ./collect.sh                # print to stdout
#   ./collect.sh -o report.txt  # and save a copy
#
# kubectl checks always run. AWS checks run only if the `aws` CLI is present and
# the state file (or the environment) supplies the names; otherwise they are
# reported as skipped rather than failing the run.
#
# Redaction: the entire report is filtered through redact(), which masks JWTs,
# AWS key ids, presigned-URL parameters, private-key headers, and the account id
# inside ARNs and ECR hostnames. It deliberately leaves the bare account id in
# the "authenticated to account" line, because confirming that is the point of
# the check. Review the report before pasting it into a ticket.

set -uo pipefail   # deliberately not -e: a failing probe is a finding, not an abort

OUT=""
while getopts ":o:h" opt; do
  case "$opt" in
    o) OUT="$OPTARG" ;;
    h) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "usage: $0 [-o report.txt]" >&2; exit 2 ;;
  esac
done

AGENT_NS="${AGENT_NS:-dataplane-agent}"
: "${UNION_ENV_FILE:=$PWD/.union-selfserve.env}"
# shellcheck disable=SC1090
[ -f "$UNION_ENV_FILE" ] && . "$UNION_ENV_FILE"

PASS=0; WARN=0; FAIL=0

hr()   { printf '\n=== %s %s\n' "$1" "$(printf '%.0s-' $(seq 1 $((66 - ${#1}))))"; }
ok()   { PASS=$((PASS+1)); printf '  [ ok ] %s\n' "$1"; }
warn() { WARN=$((WARN+1)); printf '  [warn] %s\n' "$1"; }
fail() { FAIL=$((FAIL+1)); printf '  [FAIL] %s\n' "$1"; }
skip() { printf '  [skip] %s\n' "$1"; }

# Mask anything that looks like a bearer token, key, presigned URL or account id.
# Kept to constructs both GNU and BSD sed accept: no \b word boundaries and no
# `I` flag, because this has to work unchanged on a macOS laptop.
redact() {
  sed -E \
    -e 's/(eyJ[A-Za-z0-9_-]{10,})[A-Za-z0-9._-]*/\1<REDACTED-JWT>/g' \
    -e 's/([Xx]-[Aa]mz-([Ss]ignature|[Cc]redential|[Ss]ecurity-[Tt]oken)=)[^&" ]*/\1<REDACTED>/g' \
    -e 's/(AKIA|ASIA)[A-Z0-9]{8,}/\1<REDACTED>/g' \
    -e 's/-----BEGIN [A-Z ]*PRIVATE KEY-----/<REDACTED-PRIVATE-KEY>/g' \
    -e 's/(arn:aws[a-z-]*:[a-z0-9-]*:[a-z0-9-]*:)[0-9]{12}:/\1<ACCOUNT-ID>:/g' \
    -e 's/[0-9]{12}(\.dkr\.ecr\.)/<ACCOUNT-ID>\1/g' \
    -e 's/(oidc\.eks\.[a-z0-9-]*\.amazonaws\.com\/id\/)[A-F0-9]{16,}/\1<OIDC-ID>/g'
}

have() { command -v "$1" >/dev/null 2>&1; }

main() {
  printf 'Union.ai data plane health snapshot\n'
  printf 'date: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  printf 'state file: %s%s\n' "$UNION_ENV_FILE" "$([ -f "$UNION_ENV_FILE" ] || echo ' (absent)')"

  # ---------------------------------------------------------------- cluster
  hr "cluster access"
  if ! have kubectl; then
    fail "kubectl is not installed — no cluster checks are possible"
    printf '\nsummary: %d ok, %d warn, %d fail\n' "$PASS" "$WARN" "$FAIL"
    return 1
  fi
  CTX=$(kubectl config current-context 2>/dev/null)
  if [ -z "$CTX" ]; then
    fail "no current kubectl context"
  else
    ok "context: $CTX"
    if [ -n "${CLUSTER_NAME:-}" ]; then
      case "$CTX" in
        *"$CLUSTER_NAME"*) ok "context matches CLUSTER_NAME=$CLUSTER_NAME" ;;
        *) warn "context does not mention CLUSTER_NAME=$CLUSTER_NAME — wrong cluster?" ;;
      esac
    fi
  fi
  if kubectl get --raw='/readyz' >/dev/null 2>&1 || kubectl get ns >/dev/null 2>&1; then
    ok "API server reachable"
  else
    fail "API server unreachable: $(kubectl get ns 2>&1 | grep -v '^$' | head -2 | tr '\n' ' ')"
    printf '\nsummary: %d ok, %d warn, %d fail\n' "$PASS" "$WARN" "$FAIL"
    return 1
  fi

  # ------------------------------------------------------------------ nodes
  hr "nodes"
  NODES=$(kubectl get nodes --no-headers 2>/dev/null | wc -l | tr -d ' ')
  READY=$(kubectl get nodes --no-headers 2>/dev/null | awk '$2=="Ready"' | wc -l | tr -d ' ')
  if [ "$NODES" = "0" ]; then
    warn "no nodes — normal on an idle Auto Mode cluster, a problem if pods are Pending"
  elif [ "$READY" = "$NODES" ]; then
    ok "$READY/$NODES nodes Ready"
  else
    fail "$READY/$NODES nodes Ready"
  fi
  kubectl get nodes -o wide 2>/dev/null | head -12 | sed 's/^/    /' | redact

  # ------------------------------------------------------------------ agent
  hr "union agent (ns: $AGENT_NS)"
  if ! kubectl get ns "$AGENT_NS" >/dev/null 2>&1; then
    fail "namespace $AGENT_NS does not exist — the agent install has not run"
  else
    ok "namespace $AGENT_NS exists"
    kubectl -n "$AGENT_NS" get pods -o wide 2>/dev/null | sed 's/^/    /' | redact
    NOT_RUNNING=$(kubectl -n "$AGENT_NS" get pods --no-headers 2>/dev/null \
      | awk '$3!="Running" && $3!="Completed" {print $1" "$3}')
    if [ -z "$NOT_RUNNING" ]; then
      TOTAL=$(kubectl -n "$AGENT_NS" get pods --no-headers 2>/dev/null | wc -l | tr -d ' ')
      [ "$TOTAL" = "0" ] && fail "no pods in $AGENT_NS" || ok "all $TOTAL agent pods Running"
    else
      fail "agent pods not Running:"; printf '%s\n' "$NOT_RUNNING" | sed 's/^/      /'
    fi
    if have helm; then
      helm -n "$AGENT_NS" list 2>/dev/null | sed 's/^/    /'
    fi
    printf '  --- recent agent logs (redacted) ---\n'
    LOGS=$(kubectl -n "$AGENT_NS" logs -l app.kubernetes.io/name=dp-agent \
             --tail=25 --all-containers 2>/dev/null)
    if [ -z "$LOGS" ]; then
      # The chart's labels vary by version, so fall back to the first pod by name.
      POD=$(kubectl -n "$AGENT_NS" get pods --no-headers -o custom-columns=:metadata.name \
              2>/dev/null | head -1)
      [ -n "$POD" ] && LOGS=$(kubectl -n "$AGENT_NS" logs "$POD" --tail=25 --all-containers 2>/dev/null)
    fi
    if [ -n "$LOGS" ]; then
      printf '%s\n' "$LOGS" | redact | sed 's/^/    /'
    else
      printf '    (no logs available)\n'
    fi
  fi

  # ------------------------------------------------------------- data plane
  hr "data plane"
  DP_NS=$(kubectl get ns -o name 2>/dev/null | grep '^namespace/instance-' | head -1 | cut -d/ -f2)
  if [ -z "$DP_NS" ]; then
    warn "no instance-* namespace yet — Union.ai has not installed the data plane"
    warn "  expected for the first minutes after the agent connects; a problem after ~15"
  else
    ok "data plane namespace: $DP_NS"
    kubectl -n "$DP_NS" get pods -o wide 2>/dev/null | sed 's/^/    /' | redact
    BAD=$(kubectl -n "$DP_NS" get pods --no-headers 2>/dev/null \
      | awk '$3!="Running" && $3!="Completed" {print $1" "$3" "$4}')
    if [ -z "$BAD" ]; then
      ok "all data plane pods Running"
    else
      fail "data plane pods unhealthy:"; printf '%s\n' "$BAD" | sed 's/^/      /'
      for p in $(printf '%s\n' "$BAD" | awk '{print $1}' | head -3); do
        printf '  --- describe %s (events only) ---\n' "$p"
        kubectl -n "$DP_NS" describe pod "$p" 2>/dev/null \
          | sed -n '/^Events:/,$p' | head -20 | redact | sed 's/^/      /'
      done
    fi
    SA=$(kubectl -n "$DP_NS" get sa union-system -o jsonpath='{.metadata.annotations.eks\.amazonaws\.com/role-arn}' 2>/dev/null)
    if [ -n "$SA" ]; then
      ok "union-system service account annotated: $SA"
      if [ -n "${SYSTEM_IAM_ROLE_ARN:-}" ] && [ "$SA" != "$SYSTEM_IAM_ROLE_ARN" ]; then
        fail "  annotation differs from SYSTEM_IAM_ROLE_ARN=$SYSTEM_IAM_ROLE_ARN"
      fi
    else
      warn "union-system service account has no eks.amazonaws.com/role-arn annotation"
    fi
  fi

  # --------------------------------------------------------- metrics server
  hr "metrics server collision"
  MS=$(kubectl get deploy -A --no-headers 2>/dev/null | awk '$2 ~ /metrics-server/ {print $1"/"$2}')
  COUNT=$(printf '%s' "$MS" | grep -c . || true)
  if [ "$COUNT" -le 1 ]; then
    ok "${COUNT} metrics-server deployment(s)${MS:+: $MS}"
  else
    fail "$COUNT metrics-server deployments — they collide:"
    printf '%s\n' "$MS" | sed 's/^/      /'
  fi

  # ------------------------------------------------------- cluster-wide bad pods
  hr "pods needing attention (all namespaces)"
  BADALL=$(kubectl get pods -A --no-headers 2>/dev/null \
    | awk '$4!="Running" && $4!="Completed" {print "      "$1"/"$2"  "$4"  restarts="$5}')
  if [ -z "$BADALL" ]; then ok "none"; else printf '%s\n' "$BADALL"; warn "see above"; fi

  hr "recent warning events"
  kubectl get events -A --field-selector type=Warning \
    --sort-by=.lastTimestamp 2>/dev/null | tail -15 | redact | sed 's/^/    /' \
    || printf '    (none)\n'

  # -------------------------------------------------------------------- AWS
  hr "aws"
  if ! have aws; then
    skip "aws CLI not installed"
  else
    ACC=$(aws sts get-caller-identity --query Account --output text 2>/dev/null)
    if [ -z "$ACC" ]; then
      fail "aws sts get-caller-identity failed — credentials expired or absent"
    else
      ok "authenticated to account ${ACC}"
      if [ -n "${AWS_ACCOUNT_ID:-}" ] && [ "$ACC" != "$AWS_ACCOUNT_ID" ]; then
        fail "  shell is in $ACC but the state file says $AWS_ACCOUNT_ID"
      fi
    fi

    if [ -n "${METADATA_BUCKET:-}" ]; then
      if aws s3api head-bucket --bucket "$METADATA_BUCKET" >/dev/null 2>&1; then
        ok "bucket $METADATA_BUCKET reachable"
        aws s3api get-bucket-cors --bucket "$METADATA_BUCKET" >/dev/null 2>&1 \
          && ok "  bucket CORS configured" \
          || fail "  no CORS rule — the UI cannot fetch code or artifacts"
      else
        fail "bucket $METADATA_BUCKET not reachable from this identity"
      fi
    else
      skip "METADATA_BUCKET unset"
    fi

    if [ -n "${CLUSTER_NAME:-}" ] && [ -n "${AWS_REGION:-}" ]; then
      OIDC=$(aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
        --query 'cluster.identity.oidc.issuer' --output text 2>/dev/null | sed 's|https://||')
      if [ -n "$OIDC" ] && [ "$OIDC" != "None" ]; then
        ok "cluster OIDC issuer: $OIDC"
        aws iam list-open-id-connect-providers --output text 2>/dev/null | grep -q "${OIDC##*/}" \
          && ok "  IAM OIDC provider registered" \
          || fail "  no IAM OIDC provider for this issuer — IRSA cannot work"
      else
        fail "could not read the cluster OIDC issuer"
      fi
      STATUS=$(aws eks describe-cluster --region "$AWS_REGION" --name "$CLUSTER_NAME" \
        --query 'cluster.status' --output text 2>/dev/null)
      [ "$STATUS" = "ACTIVE" ] && ok "EKS cluster ACTIVE" || fail "EKS cluster status: ${STATUS:-unknown}"
    else
      skip "CLUSTER_NAME/AWS_REGION unset"
    fi

    for pair in "${SYSTEM_ROLE_NAME:-}:system" "${TASK_ROLE_NAME:-}:task"; do
      name="${pair%%:*}"; label="${pair##*:}"
      [ -z "$name" ] && { skip "${label} role name unset"; continue; }
      TRUST=$(aws iam get-role --role-name "$name" \
        --query 'Role.AssumeRolePolicyDocument' --output json 2>/dev/null)
      if [ -z "$TRUST" ]; then
        fail "$label role $name not found"
      elif [ -n "${OIDC:-}" ] && printf '%s' "$TRUST" | grep -q "${OIDC##*/}"; then
        ok "$label role $name trusts this cluster's OIDC provider"
      else
        fail "$label role $name does NOT trust this cluster's OIDC provider"
      fi
    done

    if [ -n "${ECR_REPO_NAME:-}" ] && [ -n "${AWS_REGION:-}" ]; then
      aws ecr get-repository-policy --region "$AWS_REGION" --repository-name "$ECR_REPO_NAME" \
        >/dev/null 2>&1 && ok "ECR repository policy set on $ECR_REPO_NAME" \
        || fail "no repository policy on $ECR_REPO_NAME — image pulls will 403"
    else
      skip "ECR_REPO_NAME unset"
    fi
  fi

  hr "summary"
  printf '  %d ok, %d warn, %d fail\n' "$PASS" "$WARN" "$FAIL"
  [ "$FAIL" -gt 0 ] && printf '  → see the symptom table in SKILL.md\n'
  return 0
}

# Redact the WHOLE report rather than individual lines: the kubectl context is a
# full cluster ARN, and any per-line approach eventually misses one.
if [ -n "$OUT" ]; then
  main 2>&1 | redact | tee "$OUT"
  printf '\nsaved to %s\n' "$OUT"
else
  main 2>&1 | redact
fi
