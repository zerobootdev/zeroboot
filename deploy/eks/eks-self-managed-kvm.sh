#!/usr/bin/env bash
# deploy/eks/eks-self-managed-kvm.sh
#
# Creates a self-managed EKS node group with nested virtualization enabled.
#
# WHY SELF-MANAGED?
# EKS Managed Node Groups silently drop CpuOptions when generating their
# internal Launch Template — even when you supply CpuOptions in your own LT.
# Self-managed ASG + Launch Template bypasses EKS entirely, so CpuOptions
# (including NestedVirtualization=enabled) is applied directly to the instance.
#
# USAGE:
#   export AWS_PROFILE=your-profile
#   export CLUSTER_NAME=zeroboot-eks
#   export REGION=ap-southeast-1
#   bash eks-self-managed-kvm.sh
#
# REQUIREMENTS:
#   - aws cli v2
#   - kubectl configured for the target cluster
#   - eksctl (for cluster-only creation, see eks-cluster-only.yaml)
#
# WHAT THIS SCRIPT DOES:
#   1. Creates IAM role + instance profile for worker nodes
#   2. Registers node role with EKS (access entry)
#   3. Fetches cluster params (endpoint, cert, subnets, SGs)
#   4. Queries latest EKS-optimized AL2023 AMI
#   5. Creates Launch Template with CpuOptions.NestedVirtualization=enabled
#   6. Creates Auto Scaling Group (2-4 nodes)
#   7. Verifies /dev/kvm is present on nodes

set -euo pipefail

: "${CLUSTER_NAME:=zeroboot-eks}"
: "${REGION:=ap-southeast-1}"
: "${INSTANCE_TYPE:=c8i.xlarge}"
: "${K8S_VERSION:=1.31}"
: "${MIN_SIZE:=1}"
: "${MAX_SIZE:=4}"
: "${DESIRED:=2}"
: "${NODE_ROLE_NAME:=zeroboot-eks-node-role}"
: "${INSTANCE_PROFILE_NAME:=zeroboot-eks-node-profile}"
: "${LT_NAME:=zeroboot-kvm-nested-virt}"
: "${ASG_NAME:=zeroboot-kvm-self-managed}"

echo "==> Fetching cluster info..."
ENDPOINT=$(aws eks describe-cluster --name "$CLUSTER_NAME" --region "$REGION" \
  --query "cluster.endpoint" --output text)
CERT_AUTH=$(aws eks describe-cluster --name "$CLUSTER_NAME" --region "$REGION" \
  --query "cluster.certificateAuthority.data" --output text)
CIDR=$(aws eks describe-cluster --name "$CLUSTER_NAME" --region "$REGION" \
  --query "cluster.kubernetesNetworkConfig.serviceIpv4Cidr" --output text)
CLUSTER_SG=$(aws eks describe-cluster --name "$CLUSTER_NAME" --region "$REGION" \
  --query "cluster.resourcesVpcConfig.clusterSecurityGroupId" --output text)
SUBNETS=$(aws eks describe-cluster --name "$CLUSTER_NAME" --region "$REGION" \
  --query "cluster.resourcesVpcConfig.subnetIds" --output text | tr '\t' ',')
ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)

echo "    Cluster:    $CLUSTER_NAME"
echo "    Region:     $REGION"
echo "    Account:    $ACCOUNT_ID"
echo "    ClusterSG:  $CLUSTER_SG"
echo "    Subnets:    $SUBNETS"

# ─── Step 1: IAM role ─────────────────────────────────────────────────────────
echo ""
echo "==> Creating IAM node role: $NODE_ROLE_NAME"

if aws iam get-role --role-name "$NODE_ROLE_NAME" &>/dev/null; then
  echo "    Role already exists, skipping."
else
  aws iam create-role \
    --role-name "$NODE_ROLE_NAME" \
    --assume-role-policy-document '{
      "Version":"2012-10-17",
      "Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]
    }' > /dev/null

  for POLICY in AmazonEKSWorkerNodePolicy AmazonEKS_CNI_Policy AmazonEC2ContainerRegistryReadOnly; do
    aws iam attach-role-policy \
      --role-name "$NODE_ROLE_NAME" \
      --policy-arn "arn:aws:iam::aws:policy/${POLICY}"
  done
  echo "    Role created."
fi

# Instance profile
if aws iam get-instance-profile --instance-profile-name "$INSTANCE_PROFILE_NAME" &>/dev/null; then
  echo "    Instance profile already exists, skipping."
else
  aws iam create-instance-profile --instance-profile-name "$INSTANCE_PROFILE_NAME" > /dev/null
  aws iam add-role-to-instance-profile \
    --instance-profile-name "$INSTANCE_PROFILE_NAME" \
    --role-name "$NODE_ROLE_NAME"
  echo "    Instance profile created. Waiting 15s for IAM propagation..."
  sleep 15
fi

NODE_ROLE_ARN="arn:aws:iam::${ACCOUNT_ID}:role/${NODE_ROLE_NAME}"
INSTANCE_PROFILE_ARN="arn:aws:iam::${ACCOUNT_ID}:instance-profile/${INSTANCE_PROFILE_NAME}"

# ─── Step 2: EKS access entry ────────────────────────────────────────────────
echo ""
echo "==> Registering node role with EKS cluster..."
aws eks create-access-entry \
  --cluster-name "$CLUSTER_NAME" \
  --principal-arn "$NODE_ROLE_ARN" \
  --type EC2_LINUX \
  --region "$REGION" 2>/dev/null || echo "    Access entry already exists."

# ─── Step 3: AMI ─────────────────────────────────────────────────────────────
echo ""
echo "==> Fetching latest EKS-optimized AMI (AL2023, K8s ${K8S_VERSION})..."
AMI_ID=$(aws ssm get-parameter \
  --name "/aws/service/eks/optimized-ami/${K8S_VERSION}/amazon-linux-2023/x86_64/standard/recommended/image_id" \
  --region "$REGION" --query "Parameter.Value" --output text)
echo "    AMI: $AMI_ID"

# ─── Step 4: UserData (AL2023 nodeadm format) ────────────────────────────────
echo ""
echo "==> Preparing UserData..."
USERDATA=$(cat << EOF
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary="//"

--//
Content-Type: application/node.eks.aws

---
apiVersion: node.eks.aws/v1alpha1
kind: NodeConfig
spec:
  cluster:
    apiServerEndpoint: ${ENDPOINT}
    certificateAuthority: ${CERT_AUTH}
    cidr: ${CIDR}
    name: ${CLUSTER_NAME}
  kubelet:
    config:
      maxPods: 110
    flags:
    - "--node-labels=kvm-capable=true,workload=zeroboot"

--//--
EOF
)
USERDATA_B64=$(echo "$USERDATA" | base64 -w 0)

# ─── Step 5: Launch Template ──────────────────────────────────────────────────
echo ""
echo "==> Creating Launch Template: $LT_NAME"
echo "    (CpuOptions.NestedVirtualization=enabled — this is the key field that"
echo "     EKS managed node groups silently drop)"

LT_DATA=$(cat << EOF
{
  "ImageId": "${AMI_ID}",
  "InstanceType": "${INSTANCE_TYPE}",
  "CpuOptions": {"NestedVirtualization": "enabled"},
  "SecurityGroupIds": ["${CLUSTER_SG}"],
  "MetadataOptions": {"HttpTokens": "required", "HttpPutResponseHopLimit": 2},
  "IamInstanceProfile": {"Arn": "${INSTANCE_PROFILE_ARN}"},
  "UserData": "${USERDATA_B64}",
  "TagSpecifications": [{
    "ResourceType": "instance",
    "Tags": [
      {"Key": "Name", "Value": "zeroboot-kvm-node"},
      {"Key": "kubernetes.io/cluster/${CLUSTER_NAME}", "Value": "owned"},
      {"Key": "kvm-capable", "Value": "true"}
    ]
  }]
}
EOF
)

LT_RESULT=$(aws ec2 create-launch-template \
  --launch-template-name "$LT_NAME" \
  --region "$REGION" \
  --launch-template-data "$LT_DATA" \
  --output json 2>/dev/null || \
  aws ec2 describe-launch-templates \
    --launch-template-names "$LT_NAME" \
    --region "$REGION" \
    --query "LaunchTemplates[0]" --output json)

LT_ID=$(echo "$LT_RESULT" | python3 -c "
import json,sys
d = json.load(sys.stdin)
# handle both create and describe responses
print(d.get('LaunchTemplate', d).get('LaunchTemplateId'))
")
LT_VERSION=$(aws ec2 describe-launch-template-versions \
  --launch-template-id "$LT_ID" --region "$REGION" \
  --query "LaunchTemplateVersions[-1].VersionNumber" --output text)

echo "    LT ID:      $LT_ID"
echo "    LT Version: $LT_VERSION"

# ─── Step 6: Auto Scaling Group ───────────────────────────────────────────────
echo ""
echo "==> Creating Auto Scaling Group: $ASG_NAME"
aws autoscaling create-auto-scaling-group \
  --auto-scaling-group-name "$ASG_NAME" \
  --launch-template "LaunchTemplateId=${LT_ID},Version=${LT_VERSION}" \
  --min-size "$MIN_SIZE" \
  --max-size "$MAX_SIZE" \
  --desired-capacity "$DESIRED" \
  --vpc-zone-identifier "$SUBNETS" \
  --tags \
    "Key=Name,Value=zeroboot-kvm-node,PropagateAtLaunch=true" \
    "Key=kubernetes.io/cluster/${CLUSTER_NAME},Value=owned,PropagateAtLaunch=true" \
    "Key=kvm-capable,Value=true,PropagateAtLaunch=true" \
  --region "$REGION" 2>/dev/null || echo "    ASG already exists."

echo "    ASG created. Waiting for nodes to join (up to 3 minutes)..."
sleep 60

# ─── Step 7: Verify ───────────────────────────────────────────────────────────
echo ""
echo "==> Verifying nodes..."
kubectl get nodes -l kvm-capable=true 2>/dev/null || echo "    (kubectl not configured or nodes not yet ready)"

echo ""
echo "==> Testing /dev/kvm access..."
kubectl run kvm-verify --restart=Never \
  --image=amazonlinux:2023 \
  --overrides='{"spec":{"nodeSelector":{"kvm-capable":"true"},"containers":[{"name":"c","image":"amazonlinux:2023","command":["sh","-c","ls -la /dev/kvm && grep -c vmx /proc/cpuinfo && cat /sys/module/kvm_intel/parameters/nested 2>/dev/null || echo N/A"],"securityContext":{"privileged":true}}]}}' \
  2>/dev/null || true

echo "    Waiting 30s for pod to start..."
sleep 30
kubectl logs kvm-verify 2>/dev/null || echo "    Pod not ready yet, check manually: kubectl logs kvm-verify"
kubectl delete pod kvm-verify --ignore-not-found 2>/dev/null

echo ""
echo "==> Done! Self-managed node group with nested virtualization created."
echo "    - Launch Template: $LT_ID (v${LT_VERSION}) — CpuOptions.NestedVirtualization=enabled"
echo "    - ASG: $ASG_NAME"
echo "    - Node label: kvm-capable=true (already set via --node-labels in userdata)"
echo ""
echo "    Next: Deploy zeroboot using deploy/k8s/"
echo "      kubectl apply -f deploy/k8s/namespace.yaml"
echo "      kubectl apply -f deploy/k8s/"
