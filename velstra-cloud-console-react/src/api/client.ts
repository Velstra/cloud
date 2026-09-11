// Generated from ../../docs/openapi.json — one function per operation, so that
// "every API call reachable from the UI" is a thing a test can check by
// name rather than a claim. Regenerate with scripts/gen-client.py; do not edit.
//
// It used to be generated from a *third* copy of the document, checked in here
// beside it, which nothing generated and nothing checked. It had drifted: four
// operations the API serves were absent from it, so they were absent from the
// operation list too — and `scripts/coverage.mjs` measured "every operation the
// API documents is reachable from this UI" against that list, which made the
// claim about a document that was not the API's. The copy is gone; the document
// in `docs/` is kept honest by `velstra-cloud-api/tests/openapi.rs`, and so is
// `operations.json`, by the same test.

import { call } from "./transport";

export type Op = { id: string; method: string; path: string; summary: string; tags: string[] };

/** Every operation the API documents, for the coverage check. */
export const OPERATIONS: Op[] = [
 {
  "id": "list-attachments",
  "method": "GET",
  "path": "/api/v1/projects/{project}/attachments",
  "summary": "List Attachments",
  "tags": [
   "Attachments"
  ]
 },
 {
  "id": "create-attachments",
  "method": "POST",
  "path": "/api/v1/projects/{project}/attachments",
  "summary": "Create one of Attachments",
  "tags": [
   "Attachments"
  ]
 },
 {
  "id": "delete-attachments",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/attachments/{name}",
  "summary": "Delete one of Attachments",
  "tags": [
   "Attachments"
  ]
 },
 {
  "id": "get-attachments",
  "method": "GET",
  "path": "/api/v1/projects/{project}/attachments/{name}",
  "summary": "Read one of Attachments",
  "tags": [
   "Attachments"
  ]
 },
 {
  "id": "patch-attachments",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/attachments/{name}",
  "summary": "Change the spec of one of Attachments",
  "tags": [
   "Attachments"
  ]
 },
 {
  "id": "reportStatus-attachments",
  "method": "POST",
  "path": "/api/v1/projects/{project}/attachments/{name}:reportStatus",
  "summary": "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
  "tags": [
   "Attachments"
  ]
 },
 {
  "id": "list-audit",
  "method": "GET",
  "path": "/api/v1/audit",
  "summary": "List Audit",
  "tags": [
   "Audit"
  ]
 },
 {
  "id": "delete-audit",
  "method": "DELETE",
  "path": "/api/v1/audit/{name}",
  "summary": "Delete one of Audit",
  "tags": [
   "Audit"
  ]
 },
 {
  "id": "get-audit",
  "method": "GET",
  "path": "/api/v1/audit/{name}",
  "summary": "Read one of Audit",
  "tags": [
   "Audit"
  ]
 },
 {
  "id": "list-bgp-peers",
  "method": "GET",
  "path": "/api/v1/bgp-peers",
  "summary": "List BGP peers",
  "tags": [
   "BGP peers"
  ]
 },
 {
  "id": "create-bgp-peers",
  "method": "POST",
  "path": "/api/v1/bgp-peers",
  "summary": "Create one of BGP peers",
  "tags": [
   "BGP peers"
  ]
 },
 {
  "id": "delete-bgp-peers",
  "method": "DELETE",
  "path": "/api/v1/bgp-peers/{name}",
  "summary": "Delete one of BGP peers",
  "tags": [
   "BGP peers"
  ]
 },
 {
  "id": "get-bgp-peers",
  "method": "GET",
  "path": "/api/v1/bgp-peers/{name}",
  "summary": "Read one of BGP peers",
  "tags": [
   "BGP peers"
  ]
 },
 {
  "id": "patch-bgp-peers",
  "method": "PATCH",
  "path": "/api/v1/bgp-peers/{name}",
  "summary": "Change the spec of one of BGP peers",
  "tags": [
   "BGP peers"
  ]
 },
 {
  "id": "list-backup-schedules",
  "method": "GET",
  "path": "/api/v1/projects/{project}/backup-schedules",
  "summary": "List Backup schedules",
  "tags": [
   "Backup schedules"
  ]
 },
 {
  "id": "create-backup-schedules",
  "method": "POST",
  "path": "/api/v1/projects/{project}/backup-schedules",
  "summary": "Create one of Backup schedules",
  "tags": [
   "Backup schedules"
  ]
 },
 {
  "id": "delete-backup-schedules",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/backup-schedules/{name}",
  "summary": "Delete one of Backup schedules",
  "tags": [
   "Backup schedules"
  ]
 },
 {
  "id": "get-backup-schedules",
  "method": "GET",
  "path": "/api/v1/projects/{project}/backup-schedules/{name}",
  "summary": "Read one of Backup schedules",
  "tags": [
   "Backup schedules"
  ]
 },
 {
  "id": "patch-backup-schedules",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/backup-schedules/{name}",
  "summary": "Change the spec of one of Backup schedules",
  "tags": [
   "Backup schedules"
  ]
 },
 {
  "id": "list-backup-targets",
  "method": "GET",
  "path": "/api/v1/backup-targets",
  "summary": "List Backup targets",
  "tags": [
   "Backup targets"
  ]
 },
 {
  "id": "create-backup-targets",
  "method": "POST",
  "path": "/api/v1/backup-targets",
  "summary": "Create one of Backup targets",
  "tags": [
   "Backup targets"
  ]
 },
 {
  "id": "delete-backup-targets",
  "method": "DELETE",
  "path": "/api/v1/backup-targets/{name}",
  "summary": "Delete one of Backup targets",
  "tags": [
   "Backup targets"
  ]
 },
 {
  "id": "get-backup-targets",
  "method": "GET",
  "path": "/api/v1/backup-targets/{name}",
  "summary": "Read one of Backup targets",
  "tags": [
   "Backup targets"
  ]
 },
 {
  "id": "patch-backup-targets",
  "method": "PATCH",
  "path": "/api/v1/backup-targets/{name}",
  "summary": "Change the spec of one of Backup targets",
  "tags": [
   "Backup targets"
  ]
 },
 {
  "id": "list-backups",
  "method": "GET",
  "path": "/api/v1/projects/{project}/backups",
  "summary": "List Backups",
  "tags": [
   "Backups"
  ]
 },
 {
  "id": "create-backups",
  "method": "POST",
  "path": "/api/v1/projects/{project}/backups",
  "summary": "Create one of Backups",
  "tags": [
   "Backups"
  ]
 },
 {
  "id": "delete-backups",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/backups/{name}",
  "summary": "Delete one of Backups",
  "tags": [
   "Backups"
  ]
 },
 {
  "id": "get-backups",
  "method": "GET",
  "path": "/api/v1/projects/{project}/backups/{name}",
  "summary": "Read one of Backups",
  "tags": [
   "Backups"
  ]
 },
 {
  "id": "list-captures",
  "method": "GET",
  "path": "/api/v1/projects/{project}/captures",
  "summary": "List Captures",
  "tags": [
   "Captures"
  ]
 },
 {
  "id": "create-captures",
  "method": "POST",
  "path": "/api/v1/projects/{project}/captures",
  "summary": "Create one of Captures",
  "tags": [
   "Captures"
  ]
 },
 {
  "id": "delete-captures",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/captures/{name}",
  "summary": "Delete one of Captures",
  "tags": [
   "Captures"
  ]
 },
 {
  "id": "get-captures",
  "method": "GET",
  "path": "/api/v1/projects/{project}/captures/{name}",
  "summary": "Read one of Captures",
  "tags": [
   "Captures"
  ]
 },
 {
  "id": "openapi",
  "method": "GET",
  "path": "/api/v1/openapi.json",
  "summary": "This document.",
  "tags": [
   "Cell"
  ]
 },
 {
  "id": "metrics",
  "method": "GET",
  "path": "/metrics",
  "summary": "Prometheus text exposition, behind the same bearer token as everything else.",
  "tags": [
   "Cell"
  ]
 },
 {
  "id": "list-ceph-clusters",
  "method": "GET",
  "path": "/api/v1/ceph-clusters",
  "summary": "List Ceph",
  "tags": [
   "Ceph"
  ]
 },
 {
  "id": "create-ceph-clusters",
  "method": "POST",
  "path": "/api/v1/ceph-clusters",
  "summary": "Create one of Ceph",
  "tags": [
   "Ceph"
  ]
 },
 {
  "id": "delete-ceph-clusters",
  "method": "DELETE",
  "path": "/api/v1/ceph-clusters/{name}",
  "summary": "Delete one of Ceph",
  "tags": [
   "Ceph"
  ]
 },
 {
  "id": "get-ceph-clusters",
  "method": "GET",
  "path": "/api/v1/ceph-clusters/{name}",
  "summary": "Read one of Ceph",
  "tags": [
   "Ceph"
  ]
 },
 {
  "id": "patch-ceph-clusters",
  "method": "PATCH",
  "path": "/api/v1/ceph-clusters/{name}",
  "summary": "Change the spec of one of Ceph",
  "tags": [
   "Ceph"
  ]
 },
 {
  "id": "list-device-classes",
  "method": "GET",
  "path": "/api/v1/device-classes",
  "summary": "List Device classes",
  "tags": [
   "Device classes"
  ]
 },
 {
  "id": "create-device-classes",
  "method": "POST",
  "path": "/api/v1/device-classes",
  "summary": "Create one of Device classes",
  "tags": [
   "Device classes"
  ]
 },
 {
  "id": "delete-device-classes",
  "method": "DELETE",
  "path": "/api/v1/device-classes/{name}",
  "summary": "Delete one of Device classes",
  "tags": [
   "Device classes"
  ]
 },
 {
  "id": "get-device-classes",
  "method": "GET",
  "path": "/api/v1/device-classes/{name}",
  "summary": "Read one of Device classes",
  "tags": [
   "Device classes"
  ]
 },
 {
  "id": "patch-device-classes",
  "method": "PATCH",
  "path": "/api/v1/device-classes/{name}",
  "summary": "Change the spec of one of Device classes",
  "tags": [
   "Device classes"
  ]
 },
 {
  "id": "list-flavors",
  "method": "GET",
  "path": "/api/v1/flavors",
  "summary": "List Flavors",
  "tags": [
   "Flavors"
  ]
 },
 {
  "id": "create-flavors",
  "method": "POST",
  "path": "/api/v1/flavors",
  "summary": "Create one of Flavors",
  "tags": [
   "Flavors"
  ]
 },
 {
  "id": "delete-flavors",
  "method": "DELETE",
  "path": "/api/v1/flavors/{name}",
  "summary": "Delete one of Flavors",
  "tags": [
   "Flavors"
  ]
 },
 {
  "id": "get-flavors",
  "method": "GET",
  "path": "/api/v1/flavors/{name}",
  "summary": "Read one of Flavors",
  "tags": [
   "Flavors"
  ]
 },
 {
  "id": "patch-flavors",
  "method": "PATCH",
  "path": "/api/v1/flavors/{name}",
  "summary": "Change the spec of one of Flavors",
  "tags": [
   "Flavors"
  ]
 },
 {
  "id": "list-floatingips",
  "method": "GET",
  "path": "/api/v1/projects/{project}/floatingips",
  "summary": "List Floating IPs",
  "tags": [
   "Floating IPs"
  ]
 },
 {
  "id": "create-floatingips",
  "method": "POST",
  "path": "/api/v1/projects/{project}/floatingips",
  "summary": "Create one of Floating IPs",
  "tags": [
   "Floating IPs"
  ]
 },
 {
  "id": "delete-floatingips",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/floatingips/{name}",
  "summary": "Delete one of Floating IPs",
  "tags": [
   "Floating IPs"
  ]
 },
 {
  "id": "get-floatingips",
  "method": "GET",
  "path": "/api/v1/projects/{project}/floatingips/{name}",
  "summary": "Read one of Floating IPs",
  "tags": [
   "Floating IPs"
  ]
 },
 {
  "id": "patch-floatingips",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/floatingips/{name}",
  "summary": "Change the spec of one of Floating IPs",
  "tags": [
   "Floating IPs"
  ]
 },
 {
  "id": "explainReach-floatingips",
  "method": "GET",
  "path": "/api/v1/projects/{project}/floatingips/{name}:explainReach",
  "summary": "Whether the address reaches a guest, and every hop that decides it.",
  "tags": [
   "Floating IPs"
  ]
 },
 {
  "id": "list-folders",
  "method": "GET",
  "path": "/api/v1/folders",
  "summary": "List Folders",
  "tags": [
   "Folders"
  ]
 },
 {
  "id": "create-folders",
  "method": "POST",
  "path": "/api/v1/folders",
  "summary": "Create one of Folders",
  "tags": [
   "Folders"
  ]
 },
 {
  "id": "delete-folders",
  "method": "DELETE",
  "path": "/api/v1/folders/{name}",
  "summary": "Delete one of Folders",
  "tags": [
   "Folders"
  ]
 },
 {
  "id": "get-folders",
  "method": "GET",
  "path": "/api/v1/folders/{name}",
  "summary": "Read one of Folders",
  "tags": [
   "Folders"
  ]
 },
 {
  "id": "patch-folders",
  "method": "PATCH",
  "path": "/api/v1/folders/{name}",
  "summary": "Change the spec of one of Folders",
  "tags": [
   "Folders"
  ]
 },
 {
  "id": "list-image-sources",
  "method": "GET",
  "path": "/api/v1/image-sources",
  "summary": "List Image sources",
  "tags": [
   "Image sources"
  ]
 },
 {
  "id": "create-image-sources",
  "method": "POST",
  "path": "/api/v1/image-sources",
  "summary": "Create one of Image sources",
  "tags": [
   "Image sources"
  ]
 },
 {
  "id": "delete-image-sources",
  "method": "DELETE",
  "path": "/api/v1/image-sources/{name}",
  "summary": "Delete one of Image sources",
  "tags": [
   "Image sources"
  ]
 },
 {
  "id": "get-image-sources",
  "method": "GET",
  "path": "/api/v1/image-sources/{name}",
  "summary": "Read one of Image sources",
  "tags": [
   "Image sources"
  ]
 },
 {
  "id": "patch-image-sources",
  "method": "PATCH",
  "path": "/api/v1/image-sources/{name}",
  "summary": "Change the spec of one of Image sources",
  "tags": [
   "Image sources"
  ]
 },
 {
  "id": "list-images",
  "method": "GET",
  "path": "/api/v1/projects/{project}/images",
  "summary": "List Images",
  "tags": [
   "Images"
  ]
 },
 {
  "id": "create-images",
  "method": "POST",
  "path": "/api/v1/projects/{project}/images",
  "summary": "Create one of Images",
  "tags": [
   "Images"
  ]
 },
 {
  "id": "delete-images",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/images/{name}",
  "summary": "Delete one of Images",
  "tags": [
   "Images"
  ]
 },
 {
  "id": "get-images",
  "method": "GET",
  "path": "/api/v1/projects/{project}/images/{name}",
  "summary": "Read one of Images",
  "tags": [
   "Images"
  ]
 },
 {
  "id": "patch-images",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/images/{name}",
  "summary": "Change the spec of one of Images",
  "tags": [
   "Images"
  ]
 },
 {
  "id": "list-instances",
  "method": "GET",
  "path": "/api/v1/projects/{project}/instances",
  "summary": "List Instances",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "create-instances",
  "method": "POST",
  "path": "/api/v1/projects/{project}/instances",
  "summary": "Create one of Instances",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "delete-instances",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/instances/{name}",
  "summary": "Delete one of Instances",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "get-instances",
  "method": "GET",
  "path": "/api/v1/projects/{project}/instances/{name}",
  "summary": "Read one of Instances",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "patch-instances",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/instances/{name}",
  "summary": "Change the spec of one of Instances",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "console-instances",
  "method": "POST",
  "path": "/api/v1/projects/{project}/instances/{name}:console",
  "summary": "Open a console session to the guest; the answer names the session and its ticket.",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "consoleStream-instances",
  "method": "GET",
  "path": "/api/v1/projects/{project}/instances/{name}:consoleStream",
  "summary": "The console stream itself, as a WebSocket upgrade.",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "explainMigration-instances",
  "method": "GET",
  "path": "/api/v1/projects/{project}/instances/{name}:explainMigration",
  "summary": "Where the guest could move, and which node refused for what.",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "explainPlacement-instances",
  "method": "GET",
  "path": "/api/v1/projects/{project}/instances/{name}:explainPlacement",
  "summary": "Why the guest is where it is \u2014 or which rule stopped it going anywhere.",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "explainRecovery-instances",
  "method": "GET",
  "path": "/api/v1/projects/{project}/instances/{name}:explainRecovery",
  "summary": "What would happen to the guest if its node stopped answering.",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "reportStatus-instances",
  "method": "POST",
  "path": "/api/v1/projects/{project}/instances/{name}:reportStatus",
  "summary": "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
  "tags": [
   "Instances"
  ]
 },
 {
  "id": "list-load-balancers",
  "method": "GET",
  "path": "/api/v1/projects/{project}/load-balancers",
  "summary": "List Load balancers",
  "tags": [
   "Load balancers"
  ]
 },
 {
  "id": "create-load-balancers",
  "method": "POST",
  "path": "/api/v1/projects/{project}/load-balancers",
  "summary": "Create one of Load balancers",
  "tags": [
   "Load balancers"
  ]
 },
 {
  "id": "delete-load-balancers",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/load-balancers/{name}",
  "summary": "Delete one of Load balancers",
  "tags": [
   "Load balancers"
  ]
 },
 {
  "id": "get-load-balancers",
  "method": "GET",
  "path": "/api/v1/projects/{project}/load-balancers/{name}",
  "summary": "Read one of Load balancers",
  "tags": [
   "Load balancers"
  ]
 },
 {
  "id": "patch-load-balancers",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/load-balancers/{name}",
  "summary": "Change the spec of one of Load balancers",
  "tags": [
   "Load balancers"
  ]
 },
 {
  "id": "list-maintenance-windows",
  "method": "GET",
  "path": "/api/v1/maintenance-windows",
  "summary": "List Maintenance",
  "tags": [
   "Maintenance"
  ]
 },
 {
  "id": "create-maintenance-windows",
  "method": "POST",
  "path": "/api/v1/maintenance-windows",
  "summary": "Create one of Maintenance",
  "tags": [
   "Maintenance"
  ]
 },
 {
  "id": "delete-maintenance-windows",
  "method": "DELETE",
  "path": "/api/v1/maintenance-windows/{name}",
  "summary": "Delete one of Maintenance",
  "tags": [
   "Maintenance"
  ]
 },
 {
  "id": "get-maintenance-windows",
  "method": "GET",
  "path": "/api/v1/maintenance-windows/{name}",
  "summary": "Read one of Maintenance",
  "tags": [
   "Maintenance"
  ]
 },
 {
  "id": "patch-maintenance-windows",
  "method": "PATCH",
  "path": "/api/v1/maintenance-windows/{name}",
  "summary": "Change the spec of one of Maintenance",
  "tags": [
   "Maintenance"
  ]
 },
 {
  "id": "list-migrations",
  "method": "GET",
  "path": "/api/v1/projects/{project}/migrations",
  "summary": "List Migrations",
  "tags": [
   "Migrations"
  ]
 },
 {
  "id": "create-migrations",
  "method": "POST",
  "path": "/api/v1/projects/{project}/migrations",
  "summary": "Create one of Migrations",
  "tags": [
   "Migrations"
  ]
 },
 {
  "id": "delete-migrations",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/migrations/{name}",
  "summary": "Delete one of Migrations",
  "tags": [
   "Migrations"
  ]
 },
 {
  "id": "get-migrations",
  "method": "GET",
  "path": "/api/v1/projects/{project}/migrations/{name}",
  "summary": "Read one of Migrations",
  "tags": [
   "Migrations"
  ]
 },
 {
  "id": "list-networks",
  "method": "GET",
  "path": "/api/v1/projects/{project}/networks",
  "summary": "List Networks",
  "tags": [
   "Networks"
  ]
 },
 {
  "id": "create-networks",
  "method": "POST",
  "path": "/api/v1/projects/{project}/networks",
  "summary": "Create one of Networks",
  "tags": [
   "Networks"
  ]
 },
 {
  "id": "delete-networks",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/networks/{name}",
  "summary": "Delete one of Networks",
  "tags": [
   "Networks"
  ]
 },
 {
  "id": "get-networks",
  "method": "GET",
  "path": "/api/v1/projects/{project}/networks/{name}",
  "summary": "Read one of Networks",
  "tags": [
   "Networks"
  ]
 },
 {
  "id": "patch-networks",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/networks/{name}",
  "summary": "Change the spec of one of Networks",
  "tags": [
   "Networks"
  ]
 },
 {
  "id": "list-nodes",
  "method": "GET",
  "path": "/api/v1/nodes",
  "summary": "List Nodes",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "create-nodes",
  "method": "POST",
  "path": "/api/v1/nodes",
  "summary": "Create one of Nodes",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "get-nodes",
  "method": "GET",
  "path": "/api/v1/nodes/{name}",
  "summary": "Read one of Nodes",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "patch-nodes",
  "method": "PATCH",
  "path": "/api/v1/nodes/{name}",
  "summary": "Change the spec of one of Nodes",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "explainMaintenance-nodes",
  "method": "GET",
  "path": "/api/v1/nodes/{name}:explainMaintenance",
  "summary": "What taking the machine out of service would move, and where to.",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "issueCredential-nodes",
  "method": "POST",
  "path": "/api/v1/nodes/{name}:issueCredential",
  "summary": "Mint a fresh credential for a machine that already exists; shown once.",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "reportStatus-nodes",
  "method": "POST",
  "path": "/api/v1/nodes/{name}:reportStatus",
  "summary": "The machine's own report. A node identity only; `If-Match` carries the revision read.",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "explainCapacity-nodes",
  "method": "GET",
  "path": "/api/v1/nodes:explainCapacity",
  "summary": "What the cell has in silicon and what it can still schedule.",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "explainCpu-nodes",
  "method": "GET",
  "path": "/api/v1/nodes:explainCpu",
  "summary": "The cell's processor generations, and what can migrate where.",
  "tags": [
   "Nodes"
  ]
 },
 {
  "id": "list-operations",
  "method": "GET",
  "path": "/api/v1/projects/{project}/operations",
  "summary": "List Operations",
  "tags": [
   "Operations"
  ]
 },
 {
  "id": "get-operations",
  "method": "GET",
  "path": "/api/v1/projects/{project}/operations/{name}",
  "summary": "Read one of Operations",
  "tags": [
   "Operations"
  ]
 },
 {
  "id": "list-pools",
  "method": "GET",
  "path": "/api/v1/pools",
  "summary": "List Pools",
  "tags": [
   "Pools"
  ]
 },
 {
  "id": "create-pools",
  "method": "POST",
  "path": "/api/v1/pools",
  "summary": "Create one of Pools",
  "tags": [
   "Pools"
  ]
 },
 {
  "id": "get-pools",
  "method": "GET",
  "path": "/api/v1/pools/{name}",
  "summary": "Read one of Pools",
  "tags": [
   "Pools"
  ]
 },
 {
  "id": "patch-pools",
  "method": "PATCH",
  "path": "/api/v1/pools/{name}",
  "summary": "Change the spec of one of Pools",
  "tags": [
   "Pools"
  ]
 },
 {
  "id": "issueCredential-pools",
  "method": "POST",
  "path": "/api/v1/pools/{name}:issueCredential",
  "summary": "Mint a fresh credential for a pool that already exists; shown once.",
  "tags": [
   "Pools"
  ]
 },
 {
  "id": "list-ports",
  "method": "GET",
  "path": "/api/v1/projects/{project}/ports",
  "summary": "List Ports",
  "tags": [
   "Ports"
  ]
 },
 {
  "id": "create-ports",
  "method": "POST",
  "path": "/api/v1/projects/{project}/ports",
  "summary": "Create one of Ports",
  "tags": [
   "Ports"
  ]
 },
 {
  "id": "delete-ports",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/ports/{name}",
  "summary": "Delete one of Ports",
  "tags": [
   "Ports"
  ]
 },
 {
  "id": "get-ports",
  "method": "GET",
  "path": "/api/v1/projects/{project}/ports/{name}",
  "summary": "Read one of Ports",
  "tags": [
   "Ports"
  ]
 },
 {
  "id": "patch-ports",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/ports/{name}",
  "summary": "Change the spec of one of Ports",
  "tags": [
   "Ports"
  ]
 },
 {
  "id": "reportStatus-ports",
  "method": "POST",
  "path": "/api/v1/projects/{project}/ports/{name}:reportStatus",
  "summary": "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
  "tags": [
   "Ports"
  ]
 },
 {
  "id": "list-projects",
  "method": "GET",
  "path": "/api/v1/projects",
  "summary": "List Projects",
  "tags": [
   "Projects"
  ]
 },
 {
  "id": "create-projects",
  "method": "POST",
  "path": "/api/v1/projects",
  "summary": "Create one of Projects",
  "tags": [
   "Projects"
  ]
 },
 {
  "id": "delete-projects",
  "method": "DELETE",
  "path": "/api/v1/projects/{name}",
  "summary": "Delete one of Projects",
  "tags": [
   "Projects"
  ]
 },
 {
  "id": "get-projects",
  "method": "GET",
  "path": "/api/v1/projects/{name}",
  "summary": "Read one of Projects",
  "tags": [
   "Projects"
  ]
 },
 {
  "id": "patch-projects",
  "method": "PATCH",
  "path": "/api/v1/projects/{name}",
  "summary": "Change the spec of one of Projects",
  "tags": [
   "Projects"
  ]
 },
 {
  "id": "explainQuota-projects",
  "method": "GET",
  "path": "/api/v1/projects/{name}:explainQuota",
  "summary": "What the project has left, and what it could actually start.",
  "tags": [
   "Projects"
  ]
 },
 {
  "id": "explainUsage-projects",
  "method": "GET",
  "path": "/api/v1/projects/{name}:explainUsage",
  "summary": "What the project had, and when.",
  "tags": [
   "Projects"
  ]
 },
 {
  "id": "list-roles",
  "method": "GET",
  "path": "/api/v1/roles",
  "summary": "List Roles",
  "tags": [
   "Roles"
  ]
 },
 {
  "id": "create-roles",
  "method": "POST",
  "path": "/api/v1/roles",
  "summary": "Create one of Roles",
  "tags": [
   "Roles"
  ]
 },
 {
  "id": "delete-roles",
  "method": "DELETE",
  "path": "/api/v1/roles/{name}",
  "summary": "Delete one of Roles",
  "tags": [
   "Roles"
  ]
 },
 {
  "id": "get-roles",
  "method": "GET",
  "path": "/api/v1/roles/{name}",
  "summary": "Read one of Roles",
  "tags": [
   "Roles"
  ]
 },
 {
  "id": "patch-roles",
  "method": "PATCH",
  "path": "/api/v1/roles/{name}",
  "summary": "Change the spec of one of Roles",
  "tags": [
   "Roles"
  ]
 },
 {
  "id": "list-routers",
  "method": "GET",
  "path": "/api/v1/projects/{project}/routers",
  "summary": "List Routers",
  "tags": [
   "Routers"
  ]
 },
 {
  "id": "create-routers",
  "method": "POST",
  "path": "/api/v1/projects/{project}/routers",
  "summary": "Create one of Routers",
  "tags": [
   "Routers"
  ]
 },
 {
  "id": "delete-routers",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/routers/{name}",
  "summary": "Delete one of Routers",
  "tags": [
   "Routers"
  ]
 },
 {
  "id": "get-routers",
  "method": "GET",
  "path": "/api/v1/projects/{project}/routers/{name}",
  "summary": "Read one of Routers",
  "tags": [
   "Routers"
  ]
 },
 {
  "id": "patch-routers",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/routers/{name}",
  "summary": "Change the spec of one of Routers",
  "tags": [
   "Routers"
  ]
 },
 {
  "id": "list-security-groups",
  "method": "GET",
  "path": "/api/v1/projects/{project}/security-groups",
  "summary": "List Security groups",
  "tags": [
   "Security groups"
  ]
 },
 {
  "id": "create-security-groups",
  "method": "POST",
  "path": "/api/v1/projects/{project}/security-groups",
  "summary": "Create one of Security groups",
  "tags": [
   "Security groups"
  ]
 },
 {
  "id": "delete-security-groups",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/security-groups/{name}",
  "summary": "Delete one of Security groups",
  "tags": [
   "Security groups"
  ]
 },
 {
  "id": "get-security-groups",
  "method": "GET",
  "path": "/api/v1/projects/{project}/security-groups/{name}",
  "summary": "Read one of Security groups",
  "tags": [
   "Security groups"
  ]
 },
 {
  "id": "patch-security-groups",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/security-groups/{name}",
  "summary": "Change the spec of one of Security groups",
  "tags": [
   "Security groups"
  ]
 },
 {
  "id": "sign-in",
  "method": "POST",
  "path": "/api/v1/sessions",
  "summary": "Sign in with a username and password; the answer carries the token every other route wants.",
  "tags": [
   "Sessions"
  ]
 },
 {
  "id": "sign-out",
  "method": "DELETE",
  "path": "/api/v1/sessions/current",
  "summary": "Sign out: end the session this token names.",
  "tags": [
   "Sessions"
  ]
 },
 {
  "id": "whoami",
  "method": "GET",
  "path": "/api/v1/sessions/current",
  "summary": "Who this token is, and what it may do (`cellAdmin`, and the strongest rung per project).",
  "tags": [
   "Sessions"
  ]
 },
 {
  "id": "list-snapshot-schedules",
  "method": "GET",
  "path": "/api/v1/projects/{project}/snapshot-schedules",
  "summary": "List Snapshot schedules",
  "tags": [
   "Snapshot schedules"
  ]
 },
 {
  "id": "create-snapshot-schedules",
  "method": "POST",
  "path": "/api/v1/projects/{project}/snapshot-schedules",
  "summary": "Create one of Snapshot schedules",
  "tags": [
   "Snapshot schedules"
  ]
 },
 {
  "id": "delete-snapshot-schedules",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/snapshot-schedules/{name}",
  "summary": "Delete one of Snapshot schedules",
  "tags": [
   "Snapshot schedules"
  ]
 },
 {
  "id": "get-snapshot-schedules",
  "method": "GET",
  "path": "/api/v1/projects/{project}/snapshot-schedules/{name}",
  "summary": "Read one of Snapshot schedules",
  "tags": [
   "Snapshot schedules"
  ]
 },
 {
  "id": "patch-snapshot-schedules",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/snapshot-schedules/{name}",
  "summary": "Change the spec of one of Snapshot schedules",
  "tags": [
   "Snapshot schedules"
  ]
 },
 {
  "id": "list-subnets",
  "method": "GET",
  "path": "/api/v1/projects/{project}/subnets",
  "summary": "List Subnets",
  "tags": [
   "Subnets"
  ]
 },
 {
  "id": "create-subnets",
  "method": "POST",
  "path": "/api/v1/projects/{project}/subnets",
  "summary": "Create one of Subnets",
  "tags": [
   "Subnets"
  ]
 },
 {
  "id": "delete-subnets",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/subnets/{name}",
  "summary": "Delete one of Subnets",
  "tags": [
   "Subnets"
  ]
 },
 {
  "id": "get-subnets",
  "method": "GET",
  "path": "/api/v1/projects/{project}/subnets/{name}",
  "summary": "Read one of Subnets",
  "tags": [
   "Subnets"
  ]
 },
 {
  "id": "patch-subnets",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/subnets/{name}",
  "summary": "Change the spec of one of Subnets",
  "tags": [
   "Subnets"
  ]
 },
 {
  "id": "list-usage",
  "method": "GET",
  "path": "/api/v1/projects/{project}/usage",
  "summary": "List Usage",
  "tags": [
   "Usage"
  ]
 },
 {
  "id": "get-usage",
  "method": "GET",
  "path": "/api/v1/projects/{project}/usage/{name}",
  "summary": "Read one of Usage",
  "tags": [
   "Usage"
  ]
 },
 {
  "id": "list-users",
  "method": "GET",
  "path": "/api/v1/users",
  "summary": "List Users",
  "tags": [
   "Users"
  ]
 },
 {
  "id": "create-users",
  "method": "POST",
  "path": "/api/v1/users",
  "summary": "Create one of Users",
  "tags": [
   "Users"
  ]
 },
 {
  "id": "set-password",
  "method": "PUT",
  "path": "/api/v1/users/{id}/password",
  "summary": "Set a password. One's own with the current one; anybody's as a cell operator.",
  "tags": [
   "Users"
  ]
 },
 {
  "id": "mint-token",
  "method": "POST",
  "path": "/api/v1/users/{id}/tokens",
  "summary": "Mint a token for a service account. Shown once; several may exist so rotation has no gap.",
  "tags": [
   "Users"
  ]
 },
 {
  "id": "delete-users",
  "method": "DELETE",
  "path": "/api/v1/users/{name}",
  "summary": "Delete one of Users",
  "tags": [
   "Users"
  ]
 },
 {
  "id": "get-users",
  "method": "GET",
  "path": "/api/v1/users/{name}",
  "summary": "Read one of Users",
  "tags": [
   "Users"
  ]
 },
 {
  "id": "patch-users",
  "method": "PATCH",
  "path": "/api/v1/users/{name}",
  "summary": "Change the spec of one of Users",
  "tags": [
   "Users"
  ]
 },
 {
  "id": "list-volumes",
  "method": "GET",
  "path": "/api/v1/projects/{project}/volumes",
  "summary": "List Volumes",
  "tags": [
   "Volumes"
  ]
 },
 {
  "id": "create-volumes",
  "method": "POST",
  "path": "/api/v1/projects/{project}/volumes",
  "summary": "Create one of Volumes",
  "tags": [
   "Volumes"
  ]
 },
 {
  "id": "delete-volumes",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/volumes/{name}",
  "summary": "Delete one of Volumes",
  "tags": [
   "Volumes"
  ]
 },
 {
  "id": "get-volumes",
  "method": "GET",
  "path": "/api/v1/projects/{project}/volumes/{name}",
  "summary": "Read one of Volumes",
  "tags": [
   "Volumes"
  ]
 },
 {
  "id": "patch-volumes",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/volumes/{name}",
  "summary": "Change the spec of one of Volumes",
  "tags": [
   "Volumes"
  ]
 },
 {
  "id": "reportStatus-volumes",
  "method": "POST",
  "path": "/api/v1/projects/{project}/volumes/{name}:reportStatus",
  "summary": "The owning agent's report. A node identity only; `If-Match` carries the revision read.",
  "tags": [
   "Volumes"
  ]
 },
 {
  "id": "list-console-sessions",
  "method": "GET",
  "path": "/api/v1/projects/{project}/console-sessions",
  "summary": "List console-sessions",
  "tags": [
   "console-sessions"
  ]
 },
 {
  "id": "create-console-sessions",
  "method": "POST",
  "path": "/api/v1/projects/{project}/console-sessions",
  "summary": "Create one of console-sessions",
  "tags": [
   "console-sessions"
  ]
 },
 {
  "id": "delete-console-sessions",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/console-sessions/{name}",
  "summary": "Delete one of console-sessions",
  "tags": [
   "console-sessions"
  ]
 },
 {
  "id": "get-console-sessions",
  "method": "GET",
  "path": "/api/v1/projects/{project}/console-sessions/{name}",
  "summary": "Read one of console-sessions",
  "tags": [
   "console-sessions"
  ]
 },
 {
  "id": "patch-console-sessions",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/console-sessions/{name}",
  "summary": "Change the spec of one of console-sessions",
  "tags": [
   "console-sessions"
  ]
 },
 {
  "id": "list-snapshots",
  "method": "GET",
  "path": "/api/v1/projects/{project}/snapshots",
  "summary": "List snapshots",
  "tags": [
   "snapshots"
  ]
 },
 {
  "id": "create-snapshots",
  "method": "POST",
  "path": "/api/v1/projects/{project}/snapshots",
  "summary": "Create one of snapshots",
  "tags": [
   "snapshots"
  ]
 },
 {
  "id": "delete-snapshots",
  "method": "DELETE",
  "path": "/api/v1/projects/{project}/snapshots/{name}",
  "summary": "Delete one of snapshots",
  "tags": [
   "snapshots"
  ]
 },
 {
  "id": "get-snapshots",
  "method": "GET",
  "path": "/api/v1/projects/{project}/snapshots/{name}",
  "summary": "Read one of snapshots",
  "tags": [
   "snapshots"
  ]
 },
 {
  "id": "patch-snapshots",
  "method": "PATCH",
  "path": "/api/v1/projects/{project}/snapshots/{name}",
  "summary": "Change the spec of one of snapshots",
  "tags": [
   "snapshots"
  ]
 }
];

/** List Attachments — GET /api/v1/projects/{project}/attachments */
export const listAttachments = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-attachments", "GET", `/api/v1/projects/${encodeURIComponent(project)}/attachments`, query);

/** Create one of Attachments — POST /api/v1/projects/{project}/attachments */
export const createAttachments = (project: string, body?: unknown) =>
  call("create-attachments", "POST", `/api/v1/projects/${encodeURIComponent(project)}/attachments`, undefined, body);

/** Delete one of Attachments — DELETE /api/v1/projects/{project}/attachments/{name} */
export const deleteAttachments = (project: string, name: string) =>
  call("delete-attachments", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/attachments/${encodeURIComponent(name)}`, undefined);

/** Read one of Attachments — GET /api/v1/projects/{project}/attachments/{name} */
export const getAttachments = (project: string, name: string) =>
  call("get-attachments", "GET", `/api/v1/projects/${encodeURIComponent(project)}/attachments/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Attachments — PATCH /api/v1/projects/{project}/attachments/{name} */
export const patchAttachments = (project: string, name: string, body?: unknown) =>
  call("patch-attachments", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/attachments/${encodeURIComponent(name)}`, undefined, body);

/** The owning agent's report. A node identity only; `If-Match` carries the revision read. — POST /api/v1/projects/{project}/attachments/{name}:reportStatus */
export const reportStatusAttachments = (project: string, name: string, body?: unknown) =>
  call("reportStatus-attachments", "POST", `/api/v1/projects/${encodeURIComponent(project)}/attachments/${encodeURIComponent(name)}:reportStatus`, undefined, body);

/** List Audit — GET /api/v1/audit */
export const listAudit = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-audit", "GET", `/api/v1/audit`, query);

/** Delete one of Audit — DELETE /api/v1/audit/{name} */
export const deleteAudit = (name: string) =>
  call("delete-audit", "DELETE", `/api/v1/audit/${encodeURIComponent(name)}`, undefined);

/** Read one of Audit — GET /api/v1/audit/{name} */
export const getAudit = (name: string) =>
  call("get-audit", "GET", `/api/v1/audit/${encodeURIComponent(name)}`, undefined);

/** List BGP peers — GET /api/v1/bgp-peers */
export const listBgpPeers = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-bgp-peers", "GET", `/api/v1/bgp-peers`, query);

/** Create one of BGP peers — POST /api/v1/bgp-peers */
export const createBgpPeers = (body?: unknown) =>
  call("create-bgp-peers", "POST", `/api/v1/bgp-peers`, undefined, body);

/** Delete one of BGP peers — DELETE /api/v1/bgp-peers/{name} */
export const deleteBgpPeers = (name: string) =>
  call("delete-bgp-peers", "DELETE", `/api/v1/bgp-peers/${encodeURIComponent(name)}`, undefined);

/** Read one of BGP peers — GET /api/v1/bgp-peers/{name} */
export const getBgpPeers = (name: string) =>
  call("get-bgp-peers", "GET", `/api/v1/bgp-peers/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of BGP peers — PATCH /api/v1/bgp-peers/{name} */
export const patchBgpPeers = (name: string, body?: unknown) =>
  call("patch-bgp-peers", "PATCH", `/api/v1/bgp-peers/${encodeURIComponent(name)}`, undefined, body);

/** List Backup schedules — GET /api/v1/projects/{project}/backup-schedules */
export const listBackupSchedules = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-backup-schedules", "GET", `/api/v1/projects/${encodeURIComponent(project)}/backup-schedules`, query);

/** Create one of Backup schedules — POST /api/v1/projects/{project}/backup-schedules */
export const createBackupSchedules = (project: string, body?: unknown) =>
  call("create-backup-schedules", "POST", `/api/v1/projects/${encodeURIComponent(project)}/backup-schedules`, undefined, body);

/** Delete one of Backup schedules — DELETE /api/v1/projects/{project}/backup-schedules/{name} */
export const deleteBackupSchedules = (project: string, name: string) =>
  call("delete-backup-schedules", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/backup-schedules/${encodeURIComponent(name)}`, undefined);

/** Read one of Backup schedules — GET /api/v1/projects/{project}/backup-schedules/{name} */
export const getBackupSchedules = (project: string, name: string) =>
  call("get-backup-schedules", "GET", `/api/v1/projects/${encodeURIComponent(project)}/backup-schedules/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Backup schedules — PATCH /api/v1/projects/{project}/backup-schedules/{name} */
export const patchBackupSchedules = (project: string, name: string, body?: unknown) =>
  call("patch-backup-schedules", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/backup-schedules/${encodeURIComponent(name)}`, undefined, body);

/** List Backup targets — GET /api/v1/backup-targets */
export const listBackupTargets = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-backup-targets", "GET", `/api/v1/backup-targets`, query);

/** Create one of Backup targets — POST /api/v1/backup-targets */
export const createBackupTargets = (body?: unknown) =>
  call("create-backup-targets", "POST", `/api/v1/backup-targets`, undefined, body);

/** Delete one of Backup targets — DELETE /api/v1/backup-targets/{name} */
export const deleteBackupTargets = (name: string) =>
  call("delete-backup-targets", "DELETE", `/api/v1/backup-targets/${encodeURIComponent(name)}`, undefined);

/** Read one of Backup targets — GET /api/v1/backup-targets/{name} */
export const getBackupTargets = (name: string) =>
  call("get-backup-targets", "GET", `/api/v1/backup-targets/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Backup targets — PATCH /api/v1/backup-targets/{name} */
export const patchBackupTargets = (name: string, body?: unknown) =>
  call("patch-backup-targets", "PATCH", `/api/v1/backup-targets/${encodeURIComponent(name)}`, undefined, body);

/** List Backups — GET /api/v1/projects/{project}/backups */
export const listBackups = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-backups", "GET", `/api/v1/projects/${encodeURIComponent(project)}/backups`, query);

/** Create one of Backups — POST /api/v1/projects/{project}/backups */
export const createBackups = (project: string, body?: unknown) =>
  call("create-backups", "POST", `/api/v1/projects/${encodeURIComponent(project)}/backups`, undefined, body);

/** Delete one of Backups — DELETE /api/v1/projects/{project}/backups/{name} */
export const deleteBackups = (project: string, name: string) =>
  call("delete-backups", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/backups/${encodeURIComponent(name)}`, undefined);

/** Read one of Backups — GET /api/v1/projects/{project}/backups/{name} */
export const getBackups = (project: string, name: string) =>
  call("get-backups", "GET", `/api/v1/projects/${encodeURIComponent(project)}/backups/${encodeURIComponent(name)}`, undefined);

/** List Captures — GET /api/v1/projects/{project}/captures */
export const listCaptures = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-captures", "GET", `/api/v1/projects/${encodeURIComponent(project)}/captures`, query);

/** Create one of Captures — POST /api/v1/projects/{project}/captures */
export const createCaptures = (project: string, body?: unknown) =>
  call("create-captures", "POST", `/api/v1/projects/${encodeURIComponent(project)}/captures`, undefined, body);

/** Delete one of Captures — DELETE /api/v1/projects/{project}/captures/{name} */
export const deleteCaptures = (project: string, name: string) =>
  call("delete-captures", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/captures/${encodeURIComponent(name)}`, undefined);

/** Read one of Captures — GET /api/v1/projects/{project}/captures/{name} */
export const getCaptures = (project: string, name: string) =>
  call("get-captures", "GET", `/api/v1/projects/${encodeURIComponent(project)}/captures/${encodeURIComponent(name)}`, undefined);

/** This document. — GET /api/v1/openapi.json */
export const openapi = () =>
  call("openapi", "GET", `/api/v1/openapi.json`, undefined);

/** Prometheus text exposition, behind the same bearer token as everything else. — GET /metrics */
export const metrics = () =>
  call("metrics", "GET", `/metrics`, undefined);

/** List Ceph — GET /api/v1/ceph-clusters */
export const listCephClusters = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-ceph-clusters", "GET", `/api/v1/ceph-clusters`, query);

/** Create one of Ceph — POST /api/v1/ceph-clusters */
export const createCephClusters = (body?: unknown) =>
  call("create-ceph-clusters", "POST", `/api/v1/ceph-clusters`, undefined, body);

/** Delete one of Ceph — DELETE /api/v1/ceph-clusters/{name} */
export const deleteCephClusters = (name: string) =>
  call("delete-ceph-clusters", "DELETE", `/api/v1/ceph-clusters/${encodeURIComponent(name)}`, undefined);

/** Read one of Ceph — GET /api/v1/ceph-clusters/{name} */
export const getCephClusters = (name: string) =>
  call("get-ceph-clusters", "GET", `/api/v1/ceph-clusters/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Ceph — PATCH /api/v1/ceph-clusters/{name} */
export const patchCephClusters = (name: string, body?: unknown) =>
  call("patch-ceph-clusters", "PATCH", `/api/v1/ceph-clusters/${encodeURIComponent(name)}`, undefined, body);

/** List Device classes — GET /api/v1/device-classes */
export const listDeviceClasses = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-device-classes", "GET", `/api/v1/device-classes`, query);

/** Create one of Device classes — POST /api/v1/device-classes */
export const createDeviceClasses = (body?: unknown) =>
  call("create-device-classes", "POST", `/api/v1/device-classes`, undefined, body);

/** Delete one of Device classes — DELETE /api/v1/device-classes/{name} */
export const deleteDeviceClasses = (name: string) =>
  call("delete-device-classes", "DELETE", `/api/v1/device-classes/${encodeURIComponent(name)}`, undefined);

/** Read one of Device classes — GET /api/v1/device-classes/{name} */
export const getDeviceClasses = (name: string) =>
  call("get-device-classes", "GET", `/api/v1/device-classes/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Device classes — PATCH /api/v1/device-classes/{name} */
export const patchDeviceClasses = (name: string, body?: unknown) =>
  call("patch-device-classes", "PATCH", `/api/v1/device-classes/${encodeURIComponent(name)}`, undefined, body);

/** List Flavors — GET /api/v1/flavors */
export const listFlavors = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-flavors", "GET", `/api/v1/flavors`, query);

/** Create one of Flavors — POST /api/v1/flavors */
export const createFlavors = (body?: unknown) =>
  call("create-flavors", "POST", `/api/v1/flavors`, undefined, body);

/** Delete one of Flavors — DELETE /api/v1/flavors/{name} */
export const deleteFlavors = (name: string) =>
  call("delete-flavors", "DELETE", `/api/v1/flavors/${encodeURIComponent(name)}`, undefined);

/** Read one of Flavors — GET /api/v1/flavors/{name} */
export const getFlavors = (name: string) =>
  call("get-flavors", "GET", `/api/v1/flavors/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Flavors — PATCH /api/v1/flavors/{name} */
export const patchFlavors = (name: string, body?: unknown) =>
  call("patch-flavors", "PATCH", `/api/v1/flavors/${encodeURIComponent(name)}`, undefined, body);

/** List Floating IPs — GET /api/v1/projects/{project}/floatingips */
export const listFloatingips = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-floatingips", "GET", `/api/v1/projects/${encodeURIComponent(project)}/floatingips`, query);

/** Create one of Floating IPs — POST /api/v1/projects/{project}/floatingips */
export const createFloatingips = (project: string, body?: unknown) =>
  call("create-floatingips", "POST", `/api/v1/projects/${encodeURIComponent(project)}/floatingips`, undefined, body);

/** Delete one of Floating IPs — DELETE /api/v1/projects/{project}/floatingips/{name} */
export const deleteFloatingips = (project: string, name: string) =>
  call("delete-floatingips", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/floatingips/${encodeURIComponent(name)}`, undefined);

/** Read one of Floating IPs — GET /api/v1/projects/{project}/floatingips/{name} */
export const getFloatingips = (project: string, name: string) =>
  call("get-floatingips", "GET", `/api/v1/projects/${encodeURIComponent(project)}/floatingips/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Floating IPs — PATCH /api/v1/projects/{project}/floatingips/{name} */
export const patchFloatingips = (project: string, name: string, body?: unknown) =>
  call("patch-floatingips", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/floatingips/${encodeURIComponent(name)}`, undefined, body);

/** Whether the address reaches a guest, and every hop that decides it. — GET /api/v1/projects/{project}/floatingips/{name}:explainReach */
export const explainReachFloatingips = (project: string, name: string) =>
  call("explainReach-floatingips", "GET", `/api/v1/projects/${encodeURIComponent(project)}/floatingips/${encodeURIComponent(name)}:explainReach`, undefined);

/** List Folders — GET /api/v1/folders */
export const listFolders = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-folders", "GET", `/api/v1/folders`, query);

/** Create one of Folders — POST /api/v1/folders */
export const createFolders = (body?: unknown) =>
  call("create-folders", "POST", `/api/v1/folders`, undefined, body);

/** Delete one of Folders — DELETE /api/v1/folders/{name} */
export const deleteFolders = (name: string) =>
  call("delete-folders", "DELETE", `/api/v1/folders/${encodeURIComponent(name)}`, undefined);

/** Read one of Folders — GET /api/v1/folders/{name} */
export const getFolders = (name: string) =>
  call("get-folders", "GET", `/api/v1/folders/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Folders — PATCH /api/v1/folders/{name} */
export const patchFolders = (name: string, body?: unknown) =>
  call("patch-folders", "PATCH", `/api/v1/folders/${encodeURIComponent(name)}`, undefined, body);

/** List Image sources — GET /api/v1/image-sources */
export const listImageSources = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-image-sources", "GET", `/api/v1/image-sources`, query);

/** Create one of Image sources — POST /api/v1/image-sources */
export const createImageSources = (body?: unknown) =>
  call("create-image-sources", "POST", `/api/v1/image-sources`, undefined, body);

/** Delete one of Image sources — DELETE /api/v1/image-sources/{name} */
export const deleteImageSources = (name: string) =>
  call("delete-image-sources", "DELETE", `/api/v1/image-sources/${encodeURIComponent(name)}`, undefined);

/** Read one of Image sources — GET /api/v1/image-sources/{name} */
export const getImageSources = (name: string) =>
  call("get-image-sources", "GET", `/api/v1/image-sources/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Image sources — PATCH /api/v1/image-sources/{name} */
export const patchImageSources = (name: string, body?: unknown) =>
  call("patch-image-sources", "PATCH", `/api/v1/image-sources/${encodeURIComponent(name)}`, undefined, body);

/** List Images — GET /api/v1/projects/{project}/images */
export const listImages = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-images", "GET", `/api/v1/projects/${encodeURIComponent(project)}/images`, query);

/** Create one of Images — POST /api/v1/projects/{project}/images */
export const createImages = (project: string, body?: unknown) =>
  call("create-images", "POST", `/api/v1/projects/${encodeURIComponent(project)}/images`, undefined, body);

/** Delete one of Images — DELETE /api/v1/projects/{project}/images/{name} */
export const deleteImages = (project: string, name: string) =>
  call("delete-images", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/images/${encodeURIComponent(name)}`, undefined);

/** Read one of Images — GET /api/v1/projects/{project}/images/{name} */
export const getImages = (project: string, name: string) =>
  call("get-images", "GET", `/api/v1/projects/${encodeURIComponent(project)}/images/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Images — PATCH /api/v1/projects/{project}/images/{name} */
export const patchImages = (project: string, name: string, body?: unknown) =>
  call("patch-images", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/images/${encodeURIComponent(name)}`, undefined, body);

/** List Instances — GET /api/v1/projects/{project}/instances */
export const listInstances = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-instances", "GET", `/api/v1/projects/${encodeURIComponent(project)}/instances`, query);

/** Create one of Instances — POST /api/v1/projects/{project}/instances */
export const createInstances = (project: string, body?: unknown) =>
  call("create-instances", "POST", `/api/v1/projects/${encodeURIComponent(project)}/instances`, undefined, body);

/** Delete one of Instances — DELETE /api/v1/projects/{project}/instances/{name} */
export const deleteInstances = (project: string, name: string) =>
  call("delete-instances", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}`, undefined);

/** Read one of Instances — GET /api/v1/projects/{project}/instances/{name} */
export const getInstances = (project: string, name: string) =>
  call("get-instances", "GET", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Instances — PATCH /api/v1/projects/{project}/instances/{name} */
export const patchInstances = (project: string, name: string, body?: unknown) =>
  call("patch-instances", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}`, undefined, body);

/** Open a console session to the guest; the answer names the session and its ticket. — POST /api/v1/projects/{project}/instances/{name}:console */
export const consoleInstances = (project: string, name: string, body?: unknown) =>
  call("console-instances", "POST", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}:console`, undefined, body);

/** The console stream itself, as a WebSocket upgrade. — GET /api/v1/projects/{project}/instances/{name}:consoleStream */
export const consoleStreamInstances = (project: string, name: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("consoleStream-instances", "GET", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}:consoleStream`, query);

/** Where the guest could move, and which node refused for what. — GET /api/v1/projects/{project}/instances/{name}:explainMigration */
export const explainMigrationInstances = (project: string, name: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("explainMigration-instances", "GET", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}:explainMigration`, query);

/** Why the guest is where it is — or which rule stopped it going anywhere. — GET /api/v1/projects/{project}/instances/{name}:explainPlacement */
export const explainPlacementInstances = (project: string, name: string) =>
  call("explainPlacement-instances", "GET", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}:explainPlacement`, undefined);

/** What would happen to the guest if its node stopped answering. — GET /api/v1/projects/{project}/instances/{name}:explainRecovery */
export const explainRecoveryInstances = (project: string, name: string) =>
  call("explainRecovery-instances", "GET", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}:explainRecovery`, undefined);

/** The owning agent's report. A node identity only; `If-Match` carries the revision read. — POST /api/v1/projects/{project}/instances/{name}:reportStatus */
export const reportStatusInstances = (project: string, name: string, body?: unknown) =>
  call("reportStatus-instances", "POST", `/api/v1/projects/${encodeURIComponent(project)}/instances/${encodeURIComponent(name)}:reportStatus`, undefined, body);

/** List Load balancers — GET /api/v1/projects/{project}/load-balancers */
export const listLoadBalancers = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-load-balancers", "GET", `/api/v1/projects/${encodeURIComponent(project)}/load-balancers`, query);

/** Create one of Load balancers — POST /api/v1/projects/{project}/load-balancers */
export const createLoadBalancers = (project: string, body?: unknown) =>
  call("create-load-balancers", "POST", `/api/v1/projects/${encodeURIComponent(project)}/load-balancers`, undefined, body);

/** Delete one of Load balancers — DELETE /api/v1/projects/{project}/load-balancers/{name} */
export const deleteLoadBalancers = (project: string, name: string) =>
  call("delete-load-balancers", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/load-balancers/${encodeURIComponent(name)}`, undefined);

/** Read one of Load balancers — GET /api/v1/projects/{project}/load-balancers/{name} */
export const getLoadBalancers = (project: string, name: string) =>
  call("get-load-balancers", "GET", `/api/v1/projects/${encodeURIComponent(project)}/load-balancers/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Load balancers — PATCH /api/v1/projects/{project}/load-balancers/{name} */
export const patchLoadBalancers = (project: string, name: string, body?: unknown) =>
  call("patch-load-balancers", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/load-balancers/${encodeURIComponent(name)}`, undefined, body);

/** List Maintenance — GET /api/v1/maintenance-windows */
export const listMaintenanceWindows = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-maintenance-windows", "GET", `/api/v1/maintenance-windows`, query);

/** Create one of Maintenance — POST /api/v1/maintenance-windows */
export const createMaintenanceWindows = (body?: unknown) =>
  call("create-maintenance-windows", "POST", `/api/v1/maintenance-windows`, undefined, body);

/** Delete one of Maintenance — DELETE /api/v1/maintenance-windows/{name} */
export const deleteMaintenanceWindows = (name: string) =>
  call("delete-maintenance-windows", "DELETE", `/api/v1/maintenance-windows/${encodeURIComponent(name)}`, undefined);

/** Read one of Maintenance — GET /api/v1/maintenance-windows/{name} */
export const getMaintenanceWindows = (name: string) =>
  call("get-maintenance-windows", "GET", `/api/v1/maintenance-windows/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Maintenance — PATCH /api/v1/maintenance-windows/{name} */
export const patchMaintenanceWindows = (name: string, body?: unknown) =>
  call("patch-maintenance-windows", "PATCH", `/api/v1/maintenance-windows/${encodeURIComponent(name)}`, undefined, body);

/** List Migrations — GET /api/v1/projects/{project}/migrations */
export const listMigrations = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-migrations", "GET", `/api/v1/projects/${encodeURIComponent(project)}/migrations`, query);

/** Create one of Migrations — POST /api/v1/projects/{project}/migrations */
export const createMigrations = (project: string, body: unknown) =>
  call("create-migrations", "POST", `/api/v1/projects/${encodeURIComponent(project)}/migrations`, undefined, body);

/** Delete one of Migrations — DELETE /api/v1/projects/{project}/migrations/{name} */
export const deleteMigrations = (project: string, name: string) =>
  call("delete-migrations", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/migrations/${encodeURIComponent(name)}`, undefined);

/** Read one of Migrations — GET /api/v1/projects/{project}/migrations/{name} */
export const getMigrations = (project: string, name: string) =>
  call("get-migrations", "GET", `/api/v1/projects/${encodeURIComponent(project)}/migrations/${encodeURIComponent(name)}`, undefined);

/** List Networks — GET /api/v1/projects/{project}/networks */
export const listNetworks = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-networks", "GET", `/api/v1/projects/${encodeURIComponent(project)}/networks`, query);

/** Create one of Networks — POST /api/v1/projects/{project}/networks */
export const createNetworks = (project: string, body?: unknown) =>
  call("create-networks", "POST", `/api/v1/projects/${encodeURIComponent(project)}/networks`, undefined, body);

/** Delete one of Networks — DELETE /api/v1/projects/{project}/networks/{name} */
export const deleteNetworks = (project: string, name: string) =>
  call("delete-networks", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/networks/${encodeURIComponent(name)}`, undefined);

/** Read one of Networks — GET /api/v1/projects/{project}/networks/{name} */
export const getNetworks = (project: string, name: string) =>
  call("get-networks", "GET", `/api/v1/projects/${encodeURIComponent(project)}/networks/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Networks — PATCH /api/v1/projects/{project}/networks/{name} */
export const patchNetworks = (project: string, name: string, body?: unknown) =>
  call("patch-networks", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/networks/${encodeURIComponent(name)}`, undefined, body);

/** List Nodes — GET /api/v1/nodes */
export const listNodes = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-nodes", "GET", `/api/v1/nodes`, query);

/** Create one of Nodes — POST /api/v1/nodes */
export const createNodes = (body?: unknown) =>
  call("create-nodes", "POST", `/api/v1/nodes`, undefined, body);

/** Read one of Nodes — GET /api/v1/nodes/{name} */
export const getNodes = (name: string) =>
  call("get-nodes", "GET", `/api/v1/nodes/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Nodes — PATCH /api/v1/nodes/{name} */
export const patchNodes = (name: string, body?: unknown) =>
  call("patch-nodes", "PATCH", `/api/v1/nodes/${encodeURIComponent(name)}`, undefined, body);

/** What taking the machine out of service would move, and where to. — GET /api/v1/nodes/{name}:explainMaintenance */
export const explainMaintenanceNodes = (name: string) =>
  call("explainMaintenance-nodes", "GET", `/api/v1/nodes/${encodeURIComponent(name)}:explainMaintenance`, undefined);

/** Mint a fresh credential for a machine that already exists; shown once. — POST /api/v1/nodes/{name}:issueCredential */
export const issueCredentialNodes = (name: string, body?: unknown) =>
  call("issueCredential-nodes", "POST", `/api/v1/nodes/${encodeURIComponent(name)}:issueCredential`, undefined, body);

/** The machine's own report. A node identity only; `If-Match` carries the revision read. — POST /api/v1/nodes/{name}:reportStatus */
export const reportStatusNodes = (name: string, body?: unknown) =>
  call("reportStatus-nodes", "POST", `/api/v1/nodes/${encodeURIComponent(name)}:reportStatus`, undefined, body);

/** What the cell has in silicon and what it can still schedule. — GET /api/v1/nodes:explainCapacity */
export const explainCapacityNodes = (query?: Record<string, string | number | boolean | undefined>) =>
  call("explainCapacity-nodes", "GET", `/api/v1/nodes:explainCapacity`, query);

/** The cell's processor generations, and what can migrate where. — GET /api/v1/nodes:explainCpu */
export const explainCpuNodes = () =>
  call("explainCpu-nodes", "GET", `/api/v1/nodes:explainCpu`, undefined);

/** List Operations — GET /api/v1/projects/{project}/operations */
export const listOperations = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-operations", "GET", `/api/v1/projects/${encodeURIComponent(project)}/operations`, query);

/** Read one of Operations — GET /api/v1/projects/{project}/operations/{name} */
export const getOperations = (project: string, name: string) =>
  call("get-operations", "GET", `/api/v1/projects/${encodeURIComponent(project)}/operations/${encodeURIComponent(name)}`, undefined);

/** List Pools — GET /api/v1/pools */
export const listPools = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-pools", "GET", `/api/v1/pools`, query);

/** Create one of Pools — POST /api/v1/pools */
export const createPools = (body?: unknown) =>
  call("create-pools", "POST", `/api/v1/pools`, undefined, body);

/** Read one of Pools — GET /api/v1/pools/{name} */
export const getPools = (name: string) =>
  call("get-pools", "GET", `/api/v1/pools/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Pools — PATCH /api/v1/pools/{name} */
export const patchPools = (name: string, body?: unknown) =>
  call("patch-pools", "PATCH", `/api/v1/pools/${encodeURIComponent(name)}`, undefined, body);

/** Mint a fresh credential for a pool that already exists; shown once. — POST /api/v1/pools/{name}:issueCredential */
export const issueCredentialPools = (name: string, body?: unknown) =>
  call("issueCredential-pools", "POST", `/api/v1/pools/${encodeURIComponent(name)}:issueCredential`, undefined, body);

/** List Ports — GET /api/v1/projects/{project}/ports */
export const listPorts = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-ports", "GET", `/api/v1/projects/${encodeURIComponent(project)}/ports`, query);

/** Create one of Ports — POST /api/v1/projects/{project}/ports */
export const createPorts = (project: string, body?: unknown) =>
  call("create-ports", "POST", `/api/v1/projects/${encodeURIComponent(project)}/ports`, undefined, body);

/** Delete one of Ports — DELETE /api/v1/projects/{project}/ports/{name} */
export const deletePorts = (project: string, name: string) =>
  call("delete-ports", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/ports/${encodeURIComponent(name)}`, undefined);

/** Read one of Ports — GET /api/v1/projects/{project}/ports/{name} */
export const getPorts = (project: string, name: string) =>
  call("get-ports", "GET", `/api/v1/projects/${encodeURIComponent(project)}/ports/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Ports — PATCH /api/v1/projects/{project}/ports/{name} */
export const patchPorts = (project: string, name: string, body?: unknown) =>
  call("patch-ports", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/ports/${encodeURIComponent(name)}`, undefined, body);

/** The owning agent's report. A node identity only; `If-Match` carries the revision read. — POST /api/v1/projects/{project}/ports/{name}:reportStatus */
export const reportStatusPorts = (project: string, name: string, body?: unknown) =>
  call("reportStatus-ports", "POST", `/api/v1/projects/${encodeURIComponent(project)}/ports/${encodeURIComponent(name)}:reportStatus`, undefined, body);

/** List Projects — GET /api/v1/projects */
export const listProjects = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-projects", "GET", `/api/v1/projects`, query);

/** Create one of Projects — POST /api/v1/projects */
export const createProjects = (body?: unknown) =>
  call("create-projects", "POST", `/api/v1/projects`, undefined, body);

/** Delete one of Projects — DELETE /api/v1/projects/{name} */
export const deleteProjects = (name: string) =>
  call("delete-projects", "DELETE", `/api/v1/projects/${encodeURIComponent(name)}`, undefined);

/** Read one of Projects — GET /api/v1/projects/{name} */
export const getProjects = (name: string) =>
  call("get-projects", "GET", `/api/v1/projects/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Projects — PATCH /api/v1/projects/{name} */
export const patchProjects = (name: string, body?: unknown) =>
  call("patch-projects", "PATCH", `/api/v1/projects/${encodeURIComponent(name)}`, undefined, body);

/** What the project has left, and what it could actually start. — GET /api/v1/projects/{name}:explainQuota */
export const explainQuotaProjects = (name: string) =>
  call("explainQuota-projects", "GET", `/api/v1/projects/${encodeURIComponent(name)}:explainQuota`, undefined);

/** What the project had, and when. — GET /api/v1/projects/{name}:explainUsage */
export const explainUsageProjects = (name: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("explainUsage-projects", "GET", `/api/v1/projects/${encodeURIComponent(name)}:explainUsage`, query);

/** List Roles — GET /api/v1/roles */
export const listRoles = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-roles", "GET", `/api/v1/roles`, query);

/** Create one of Roles — POST /api/v1/roles */
export const createRoles = (body?: unknown) =>
  call("create-roles", "POST", `/api/v1/roles`, undefined, body);

/** Delete one of Roles — DELETE /api/v1/roles/{name} */
export const deleteRoles = (name: string) =>
  call("delete-roles", "DELETE", `/api/v1/roles/${encodeURIComponent(name)}`, undefined);

/** Read one of Roles — GET /api/v1/roles/{name} */
export const getRoles = (name: string) =>
  call("get-roles", "GET", `/api/v1/roles/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Roles — PATCH /api/v1/roles/{name} */
export const patchRoles = (name: string, body?: unknown) =>
  call("patch-roles", "PATCH", `/api/v1/roles/${encodeURIComponent(name)}`, undefined, body);

/** List Routers — GET /api/v1/projects/{project}/routers */
export const listRouters = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-routers", "GET", `/api/v1/projects/${encodeURIComponent(project)}/routers`, query);

/** Create one of Routers — POST /api/v1/projects/{project}/routers */
export const createRouters = (project: string, body?: unknown) =>
  call("create-routers", "POST", `/api/v1/projects/${encodeURIComponent(project)}/routers`, undefined, body);

/** Delete one of Routers — DELETE /api/v1/projects/{project}/routers/{name} */
export const deleteRouters = (project: string, name: string) =>
  call("delete-routers", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/routers/${encodeURIComponent(name)}`, undefined);

/** Read one of Routers — GET /api/v1/projects/{project}/routers/{name} */
export const getRouters = (project: string, name: string) =>
  call("get-routers", "GET", `/api/v1/projects/${encodeURIComponent(project)}/routers/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Routers — PATCH /api/v1/projects/{project}/routers/{name} */
export const patchRouters = (project: string, name: string, body?: unknown) =>
  call("patch-routers", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/routers/${encodeURIComponent(name)}`, undefined, body);

/** List Security groups — GET /api/v1/projects/{project}/security-groups */
export const listSecurityGroups = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-security-groups", "GET", `/api/v1/projects/${encodeURIComponent(project)}/security-groups`, query);

/** Create one of Security groups — POST /api/v1/projects/{project}/security-groups */
export const createSecurityGroups = (project: string, body?: unknown) =>
  call("create-security-groups", "POST", `/api/v1/projects/${encodeURIComponent(project)}/security-groups`, undefined, body);

/** Delete one of Security groups — DELETE /api/v1/projects/{project}/security-groups/{name} */
export const deleteSecurityGroups = (project: string, name: string) =>
  call("delete-security-groups", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/security-groups/${encodeURIComponent(name)}`, undefined);

/** Read one of Security groups — GET /api/v1/projects/{project}/security-groups/{name} */
export const getSecurityGroups = (project: string, name: string) =>
  call("get-security-groups", "GET", `/api/v1/projects/${encodeURIComponent(project)}/security-groups/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Security groups — PATCH /api/v1/projects/{project}/security-groups/{name} */
export const patchSecurityGroups = (project: string, name: string, body?: unknown) =>
  call("patch-security-groups", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/security-groups/${encodeURIComponent(name)}`, undefined, body);

/** Sign in with a username and password; the answer carries the token every other route wants. — POST /api/v1/sessions */
export const signIn = (body?: unknown) =>
  call("sign-in", "POST", `/api/v1/sessions`, undefined, body);

/** Sign out: end the session this token names. — DELETE /api/v1/sessions/current */
export const signOut = () =>
  call("sign-out", "DELETE", `/api/v1/sessions/current`, undefined);

/** Who this token is, and what it may do (`cellAdmin`, and the strongest rung per project). — GET /api/v1/sessions/current */
export const whoami = () =>
  call("whoami", "GET", `/api/v1/sessions/current`, undefined);

/** List Snapshot schedules — GET /api/v1/projects/{project}/snapshot-schedules */
export const listSnapshotSchedules = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-snapshot-schedules", "GET", `/api/v1/projects/${encodeURIComponent(project)}/snapshot-schedules`, query);

/** Create one of Snapshot schedules — POST /api/v1/projects/{project}/snapshot-schedules */
export const createSnapshotSchedules = (project: string, body?: unknown) =>
  call("create-snapshot-schedules", "POST", `/api/v1/projects/${encodeURIComponent(project)}/snapshot-schedules`, undefined, body);

/** Delete one of Snapshot schedules — DELETE /api/v1/projects/{project}/snapshot-schedules/{name} */
export const deleteSnapshotSchedules = (project: string, name: string) =>
  call("delete-snapshot-schedules", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/snapshot-schedules/${encodeURIComponent(name)}`, undefined);

/** Read one of Snapshot schedules — GET /api/v1/projects/{project}/snapshot-schedules/{name} */
export const getSnapshotSchedules = (project: string, name: string) =>
  call("get-snapshot-schedules", "GET", `/api/v1/projects/${encodeURIComponent(project)}/snapshot-schedules/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Snapshot schedules — PATCH /api/v1/projects/{project}/snapshot-schedules/{name} */
export const patchSnapshotSchedules = (project: string, name: string, body?: unknown) =>
  call("patch-snapshot-schedules", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/snapshot-schedules/${encodeURIComponent(name)}`, undefined, body);

/** List Subnets — GET /api/v1/projects/{project}/subnets */
export const listSubnets = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-subnets", "GET", `/api/v1/projects/${encodeURIComponent(project)}/subnets`, query);

/** Create one of Subnets — POST /api/v1/projects/{project}/subnets */
export const createSubnets = (project: string, body?: unknown) =>
  call("create-subnets", "POST", `/api/v1/projects/${encodeURIComponent(project)}/subnets`, undefined, body);

/** Delete one of Subnets — DELETE /api/v1/projects/{project}/subnets/{name} */
export const deleteSubnets = (project: string, name: string) =>
  call("delete-subnets", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/subnets/${encodeURIComponent(name)}`, undefined);

/** Read one of Subnets — GET /api/v1/projects/{project}/subnets/{name} */
export const getSubnets = (project: string, name: string) =>
  call("get-subnets", "GET", `/api/v1/projects/${encodeURIComponent(project)}/subnets/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Subnets — PATCH /api/v1/projects/{project}/subnets/{name} */
export const patchSubnets = (project: string, name: string, body?: unknown) =>
  call("patch-subnets", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/subnets/${encodeURIComponent(name)}`, undefined, body);

/** List Usage — GET /api/v1/projects/{project}/usage */
export const listUsage = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-usage", "GET", `/api/v1/projects/${encodeURIComponent(project)}/usage`, query);

/** Read one of Usage — GET /api/v1/projects/{project}/usage/{name} */
export const getUsage = (project: string, name: string) =>
  call("get-usage", "GET", `/api/v1/projects/${encodeURIComponent(project)}/usage/${encodeURIComponent(name)}`, undefined);

/** List Users — GET /api/v1/users */
export const listUsers = (query?: Record<string, string | number | boolean | undefined>) =>
  call("list-users", "GET", `/api/v1/users`, query);

/** Create one of Users — POST /api/v1/users */
export const createUsers = (body?: unknown) =>
  call("create-users", "POST", `/api/v1/users`, undefined, body);

/** Set a password. One's own with the current one; anybody's as a cell operator. — PUT /api/v1/users/{id}/password */
export const setPassword = (id: string, body?: unknown) =>
  call("set-password", "PUT", `/api/v1/users/${encodeURIComponent(id)}/password`, undefined, body);

/** Mint a token for a service account. Shown once; several may exist so rotation has no gap. — POST /api/v1/users/{id}/tokens */
export const mintToken = (id: string, body?: unknown) =>
  call("mint-token", "POST", `/api/v1/users/${encodeURIComponent(id)}/tokens`, undefined, body);

/** Delete one of Users — DELETE /api/v1/users/{name} */
export const deleteUsers = (name: string) =>
  call("delete-users", "DELETE", `/api/v1/users/${encodeURIComponent(name)}`, undefined);

/** Read one of Users — GET /api/v1/users/{name} */
export const getUsers = (name: string) =>
  call("get-users", "GET", `/api/v1/users/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Users — PATCH /api/v1/users/{name} */
export const patchUsers = (name: string, body?: unknown) =>
  call("patch-users", "PATCH", `/api/v1/users/${encodeURIComponent(name)}`, undefined, body);

/** List Volumes — GET /api/v1/projects/{project}/volumes */
export const listVolumes = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-volumes", "GET", `/api/v1/projects/${encodeURIComponent(project)}/volumes`, query);

/** Create one of Volumes — POST /api/v1/projects/{project}/volumes */
export const createVolumes = (project: string, body?: unknown) =>
  call("create-volumes", "POST", `/api/v1/projects/${encodeURIComponent(project)}/volumes`, undefined, body);

/** Delete one of Volumes — DELETE /api/v1/projects/{project}/volumes/{name} */
export const deleteVolumes = (project: string, name: string) =>
  call("delete-volumes", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/volumes/${encodeURIComponent(name)}`, undefined);

/** Read one of Volumes — GET /api/v1/projects/{project}/volumes/{name} */
export const getVolumes = (project: string, name: string) =>
  call("get-volumes", "GET", `/api/v1/projects/${encodeURIComponent(project)}/volumes/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of Volumes — PATCH /api/v1/projects/{project}/volumes/{name} */
export const patchVolumes = (project: string, name: string, body?: unknown) =>
  call("patch-volumes", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/volumes/${encodeURIComponent(name)}`, undefined, body);

/** The owning agent's report. A node identity only; `If-Match` carries the revision read. — POST /api/v1/projects/{project}/volumes/{name}:reportStatus */
export const reportStatusVolumes = (project: string, name: string, body?: unknown) =>
  call("reportStatus-volumes", "POST", `/api/v1/projects/${encodeURIComponent(project)}/volumes/${encodeURIComponent(name)}:reportStatus`, undefined, body);

/** List console-sessions — GET /api/v1/projects/{project}/console-sessions */
export const listConsoleSessions = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-console-sessions", "GET", `/api/v1/projects/${encodeURIComponent(project)}/console-sessions`, query);

/** Create one of console-sessions — POST /api/v1/projects/{project}/console-sessions */
export const createConsoleSessions = (project: string, body?: unknown) =>
  call("create-console-sessions", "POST", `/api/v1/projects/${encodeURIComponent(project)}/console-sessions`, undefined, body);

/** Delete one of console-sessions — DELETE /api/v1/projects/{project}/console-sessions/{name} */
export const deleteConsoleSessions = (project: string, name: string) =>
  call("delete-console-sessions", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/console-sessions/${encodeURIComponent(name)}`, undefined);

/** Read one of console-sessions — GET /api/v1/projects/{project}/console-sessions/{name} */
export const getConsoleSessions = (project: string, name: string) =>
  call("get-console-sessions", "GET", `/api/v1/projects/${encodeURIComponent(project)}/console-sessions/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of console-sessions — PATCH /api/v1/projects/{project}/console-sessions/{name} */
export const patchConsoleSessions = (project: string, name: string, body?: unknown) =>
  call("patch-console-sessions", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/console-sessions/${encodeURIComponent(name)}`, undefined, body);

/** List snapshots — GET /api/v1/projects/{project}/snapshots */
export const listSnapshots = (project: string, query?: Record<string, string | number | boolean | undefined>) =>
  call("list-snapshots", "GET", `/api/v1/projects/${encodeURIComponent(project)}/snapshots`, query);

/** Create one of snapshots — POST /api/v1/projects/{project}/snapshots */
export const createSnapshots = (project: string, body?: unknown) =>
  call("create-snapshots", "POST", `/api/v1/projects/${encodeURIComponent(project)}/snapshots`, undefined, body);

/** Delete one of snapshots — DELETE /api/v1/projects/{project}/snapshots/{name} */
export const deleteSnapshots = (project: string, name: string) =>
  call("delete-snapshots", "DELETE", `/api/v1/projects/${encodeURIComponent(project)}/snapshots/${encodeURIComponent(name)}`, undefined);

/** Read one of snapshots — GET /api/v1/projects/{project}/snapshots/{name} */
export const getSnapshots = (project: string, name: string) =>
  call("get-snapshots", "GET", `/api/v1/projects/${encodeURIComponent(project)}/snapshots/${encodeURIComponent(name)}`, undefined);

/** Change the spec of one of snapshots — PATCH /api/v1/projects/{project}/snapshots/{name} */
export const patchSnapshots = (project: string, name: string, body?: unknown) =>
  call("patch-snapshots", "PATCH", `/api/v1/projects/${encodeURIComponent(project)}/snapshots/${encodeURIComponent(name)}`, undefined, body);
