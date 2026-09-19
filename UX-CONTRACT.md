# Console UX contract

## Product context

Operators manage the cell, Ceph and hosts; project members manage their own
workloads. English UI, local browser timezone with exact timestamp tooltips.
Accessibility target: WCAG 2.2 AA. Visual policy: [DESIGN.md](DESIGN.md).

## Business-context sources

| Domain | Source |
|---|---|
| Permissions and requests | docs/rest-contract.md; velstra-cloud-model/src/access.rs |
| Desired and observed lifecycle | README.md; velstra-cloud-model/src/reconcile.rs |
| Ceph disk safety | velstra-cloud-model/src/ceph.rs; docs/setup-guide.md |
| Audit events | velstra-cloud-model/src/audit.rs |

## Canonical UI Map

All component paths below are relative to velstra-cloud-console-react/src.

| Capability | Canonical owner | Source of truth | Allowed variants | Verification |
|---|---|---|---|---|
| Table Selection | features/Board.tsx | TanStack selection state | loaded rows | Browser selection |
| Select/Listbox | components/ui/select.tsx | Base UI | authored resource choices; native project, membership and structured-row selectors | Keyboard and popup |
| Date | features/Form.tsx | API epoch milliseconds | native datetime-local | Local timezone round trip |
| Form | features/Form.tsx | schema.json and API | create/edit | Validation and live CRUD |
| Scrollbar | index.css | DESIGN.md | stable table gutters | Computed style |
| Toast | components/ui/sonner.tsx | App.tsx | success/error/info | Live regions |
| CRUD | App.tsx and features/Detail.tsx | REST contract | create-to-detail/edit-to-detail | Live browser |

## Behavior

Creation opens the resulting resource; credentials issued once remain until
acknowledged. Editing returns to detail. Deletion requires the shared Ask dialog
and returns to the board after acceptance. HTTP acceptance is not convergence.
Use If-Match for updates/deletion. Preserve form values on errors; never retry a
mutation automatically. Buttons reject duplicate activation until completion.

Extended help is collapsed by default; disk erasure warnings remain visible.
Form errors use associated text and focus the first invalid control. Product
forms use noValidate. Shared Base UI overlays own focus and Escape handling.

## Data and navigation

The shared census feeds overview and navigation from the same snapshot. Ignore
superseded requests after project/session changes. Missing or truncated inventory
prevents an all-clear. Boards preserve their existing per-collection preferences
in the local preference store; this is the current exception to URL filters.
Overview lists are bounded and link to full boards. Resource links include project
identity in the all-projects view. Global history uses /api/v1/operations.

## Verification

Run the React production build and lint, Rust console schema checks, and browser
checks for admin/project workflows, light/dark appearance, narrow layouts, error
recovery and reduced motion. Keep deployment state and live-browser credentials
outside version control. The legacy console remains a separate compatibility UI.
