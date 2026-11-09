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

## Mutations: variants configured at runtime

A `Mutation` is the same thing as a MutantDeployment, a base Deployment
plus variants of it, but it is configured at runtime through the HTTP API
below instead of being declared in git. Use it when variants come and go
while things run. It stays a Kubernetes object, so the operator keeps no
state of its own.

```yaml
apiVersion: neiam.org/v1
kind: Mutation
metadata:
  name: browser-snap
spec:
  deployment:            # the base, as in a MutantDeployment
    apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: browser-snap
    spec: { ... }
  variants:
  - label: radxa-8-hd
    env:
      SNAP_RES: "1280x800"
    instances: 3
  - label: lilygo47
    env:
      SNAP_RES: "960x540"
    instances: 1
```

```console
$ kubectl get mutations
NAME           BASE           READY                 SECRET                AGE
browser-snap   browser-snap   2/2 variants ready    browser-snap-secret   1m
```

- Each variant becomes Deployment `<base name>-<label>`. Adding a variant
  starts one, removing it deletes it, and deleting the Mutation deletes
  them all.
- Each Mutation gets a Secret, `<mutation-name>-secret`, holding a random
  64-hex-character `token`. The operator creates it once and never rewrites
  it, and it is deleted with the Mutation. It is not injected into the
  variants' pods. Anything in the namespace can read it as an env var:

  ```yaml
  - name: BROWSER_SNAP_TOKEN
    valueFrom:
      secretKeyRef:
        name: browser-snap-secret
        key: token
        optional: true   # exists once the operator has seen the Mutation
  ```

- `status` reports the Secret, a summary, and each variant's Deployment,
  ready replicas and state: `Progressing`, `Ready`, `Invalid`, or
  `Conflict`. `Conflict` means the label repeats, or the Deployment name is
  already owned by another Mutation or MutantDeployment. The operator never
  takes over another owner's Deployment.
- If the base is invalid, nothing is applied or pruned: what is running
  keeps running until the base is fixed.

### Argo CD

A Mutation made entirely at runtime does not go in git. One whose base
should live in git, with its variants still managed at runtime, can be
synced by Argo CD, if it opts in and leaves `variants` out of its manifest:

```yaml
apiVersion: neiam.org/v1
kind: Mutation
metadata:
  name: browser-snap
  annotations:
    mutantops.neiam.org/runtime-variants: "true"
spec:
  deployment: { ... }   # no `variants`: those belong to the API
```

Argo CD only applies and diffs the fields its manifest declares, so it
neither reverts nor reports the variants the API adds. The API recognises a
synced Mutation by Argo CD's `argocd.argoproj.io/tracking-id` annotation or
`app.kubernetes.io/instance` label and never deletes one. Without the
annotation it refuses variant changes too (`readOnly: true`), because they
would belong in git.

To show Mutation health in Argo CD, add to `argocd-cm`:

```yaml
resource.customizations.health.neiam.org_Mutation: |
  hs = { status = "Progressing", message = "Waiting for the operator" }
  if obj.status ~= nil and obj.status.variants ~= nil then
    hs.status = "Healthy"
    hs.message = obj.status.summary
    for _, v in ipairs(obj.status.variants) do
      if v.state == "Invalid" or v.state == "Conflict" then
        hs.status = "Degraded"; hs.message = v.label .. ": " .. (v.message or v.state); break
      elseif v.state == "Progressing" then
        hs.status = "Progressing"
      end
    end
  end
  return hs
```

## HTTP API

The operator serves an HTTP API to create, list and delete Mutations and
their variants.

Every `/api` request needs `Authorization: Bearer <token>`, and which token
depends on the request:

- **A Mutation's own token** (`token` in its `<name>-secret`) allows getting
  that Mutation and listing, adding, scaling and removing its variants, and nothing
  else. An app in the Mutation's namespace reads it from that Secret, so it
  can manage its own Mutation's variants without credentials for anything
  else. The operator only accepts a Secret owned by the Mutation, so one
  created by someone else does not count.
- **The admin token** (`MUTANT_API_TOKEN`, optional; `operator.yml` reads it
  from the `mutantops-api` secret) also allows listing every Mutation in a
  namespace and creating or deleting a Mutation. Without it, those answer
  401.

A wrong token and a Mutation that does not exist both answer 401, so one
Mutation's token cannot probe for others. `MUTANT_API_INSECURE=true` serves
the API without auth, for local development. `MUTANT_API_ADDR` sets the
listen address (default `0.0.0.0:8080`).

```text
GET    /api/v1/namespaces/{ns}/mutations
POST   /api/v1/namespaces/{ns}/mutations                   {name, deployment, variants}
GET    /api/v1/namespaces/{ns}/mutations/{name}
DELETE /api/v1/namespaces/{ns}/mutations/{name}
GET    /api/v1/namespaces/{ns}/mutations/{name}/variants
POST   /api/v1/namespaces/{ns}/mutations/{name}/variants   {label, env, instances}
GET    /api/v1/namespaces/{ns}/mutations/{name}/variants/{label}
PATCH  /api/v1/namespaces/{ns}/mutations/{name}/variants/{label}   {instances}
DELETE /api/v1/namespaces/{ns}/mutations/{name}/variants/{label}
```

`PATCH` scales a variant in place: its Deployment's replica count changes
and the running pods are kept.

Creating answers 201, or 400 if invalid and 409 if the name or label is
taken. Deleting answers 204, or 404 if absent. Changes to an Argo-synced
Mutation answer 409. Concurrent variant edits are serialised on the
Mutation's `resourceVersion`, so none is lost.

```sh
kubectl -n mutants create secret generic mutantops-api --from-literal=token="$(openssl rand -hex 32)"
kubectl -n mutants rollout restart deploy/mutantops-operator
TOKEN=$(kubectl -n mutants get secret mutantops-api -o jsonpath='{.data.token}' | base64 -d)   # admin
API=http://mutantops-api.mutants.svc   # or: kubectl -n mutants port-forward svc/mutantops-api 8080:80
NS=$API/api/v1/namespaces/default
AUTH="Authorization: Bearer $TOKEN"

# create a Mutation from a base Deployment (deployment.json)
jq -n --slurpfile d deployment.json '{name: "web", deployment: $d[0], variants: []}' |
  curl -H "$AUTH" -H 'Content-Type: application/json' -d @- $NS/mutations
# add and remove variants: the admin token works, and so does web's own
WEB=$(kubectl -n default get secret web-secret -o jsonpath='{.data.token}' | base64 -d)
AUTH="Authorization: Bearer $WEB"
curl -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"label":"beta","env":{"FEATURE_X":"on"},"instances":2}' $NS/mutations/web/variants
curl -X PATCH -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"instances":4}' $NS/mutations/web/variants/beta
curl -X DELETE -H "$AUTH" $NS/mutations/web/variants/beta
# list
curl -H "$AUTH" $NS/mutations
curl -H "$AUTH" $NS/mutations/web/variants
```

A listed variant looks like:

```json
{
  "label": "beta",
  "env": { "FEATURE_X": "on" },
  "instances": 2,
  "readOnly": false,
  "status": { "label": "beta", "state": "Ready", "message": null, "deployment": "web-beta", "replicas": 2, "readyReplicas": 2 }
}
```

A Mutation adds its `name`, `namespace`, base `deployment`, `variants`,
`secret`, `summary`, `message` and `readOnly`.
