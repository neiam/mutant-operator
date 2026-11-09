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
Removing a mutation from the list deletes its Deployment. Deleting the
MutantDeployment deletes all of them.

## Mutation resources

Mutations can also be standalone `Mutation` resources, created at runtime
without touching the MutantDeployment. Use them when the MutantDeployment is
owned by Argo CD or another GitOps tool:

```yaml
apiVersion: neiam.org/v1
kind: Mutation
metadata:
  name: nginx-mutant-beta
spec:
  mutantDeployment: nginx-mutant   # same namespace
  label: beta
  env:
    FEATURE_X: "on"
  instances: 2
```

```console
$ kubectl get mutations
NAME                TARGET         LABEL   INSTANCES   READY   STATE   AGE
nginx-mutant-beta   nginx-mutant   beta    2           2       Ready   1m
```

- A MutantDeployment runs its inline `spec.mutations` plus every Mutation
  that targets it. Labels are unique per MutantDeployment. An inline
  mutation wins a clash, then the oldest Mutation. A Mutation that loses is
  not run and shows `State: Conflict`.
- `status` reports the generated Deployment, its ready replicas, and
  `Progressing`, `Ready`, `Invalid` or `Conflict`.
- The operator makes each Mutation a dependent (`ownerReferences`) of its
  MutantDeployment. Argo CD shows it under the MutantDeployment in the
  resource tree, and deleting the MutantDeployment deletes its Mutations.

### Argo CD

Mutations created through the API are not in git and carry no Argo CD
tracking label, so Argo CD neither prunes nor reverts them. Inline
mutations remain in git and are read-only through the API. For Argo CD to
show Mutation health, add to `argocd-cm`:

```yaml
resource.customizations.health.neiam.org_Mutation: |
  hs = { status = "Progressing", message = "Waiting for the operator" }
  if obj.status ~= nil and obj.status.state ~= nil then
    hs.message = obj.status.message or obj.status.state
    if obj.status.state == "Ready" then hs.status = "Healthy"
    elseif obj.status.state == "Progressing" then hs.status = "Progressing"
    else hs.status = "Degraded" end
  end
  return hs
```

## HTTP API

The operator serves an HTTP API to list, create and destroy mutations.
Creating one makes a Mutation resource named `<mutantdeployment>-<label>`.
Destroying one deletes it.

The API is off until `MUTANT_API_TOKEN` is set (`operator.yml` reads it from
the `mutantops-api` secret). Every `/api` request then needs
`Authorization: Bearer <token>`. `MUTANT_API_INSECURE=true` serves it without
auth. `MUTANT_API_ADDR` sets the listen address (default `0.0.0.0:8080`).

```sh
kubectl -n mutants create secret generic mutantops-api --from-literal=token="$(openssl rand -hex 32)"
kubectl -n mutants rollout restart deploy/mutantops-operator
TOKEN=$(kubectl -n mutants get secret mutantops-api -o jsonpath='{.data.token}' | base64 -d)
API=http://mutantops-api.mutants.svc   # or: kubectl -n mutants port-forward svc/mutantops-api 8080:80
MD=$API/api/v1/namespaces/default/mutantdeployments/nginx-mutant

# MutantDeployments in a namespace, with their running mutation labels
curl -H "Authorization: Bearer $TOKEN" $API/api/v1/namespaces/default/mutantdeployments
# list / get mutations: inline and resource, with live status
curl -H "Authorization: Bearer $TOKEN" $MD/mutations
curl -H "Authorization: Bearer $TOKEN" $MD/mutations/canary
# create (201; 409 if the label is taken, 400 if invalid, 404 if no such MutantDeployment)
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"label":"beta","env":{"FEATURE_X":"on"},"instances":2}' $MD/mutations
# destroy (204; 404 if absent; 409 if inline, which is changed in git instead)
curl -X DELETE -H "Authorization: Bearer $TOKEN" $MD/mutations/beta
```

A listed mutation looks like:

```json
{
  "label": "beta",
  "env": { "FEATURE_X": "on" },
  "instances": 2,
  "source": { "kind": "resource", "name": "nginx-mutant-beta" },
  "status": { "state": "Ready", "message": null, "deployment": "nginx-beta", "replicas": 2, "readyReplicas": 2 }
}
```

`source.kind` is `inline` for mutations declared in the MutantDeployment.
