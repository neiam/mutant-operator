# Mutant Operations

A Kubernetes Operator

# Motivation

Don't you wish there was a way to run x deployments with y tweaks?

## Usage

```yaml
apiVersion: neiam.org/v1
kind: MutantDeployment
metadata:
  name: nginx-mutant
spec:
  deployment:
    apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: nginx-deployment
    spec:
      replicas: 3
      selector:
        matchLabels:
          app: nginx
      template:
        metadata:
          labels:
            app: nginx
        spec:
          containers:
          - name: nginx
            image: nginx:1.21
            ports:
            - containerPort: 80
  mutations:
  - label: canary
    env:
      NGINX_VERSION: "1.22"
    instances: 1
  - label: experimental
    env:
      NGINX_EXPERIMENTAL: "true"
    instances: 3
```