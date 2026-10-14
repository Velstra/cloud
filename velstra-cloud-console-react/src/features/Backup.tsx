import { useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { bytes, idOf, nameOf, type Resource } from "@/lib/model";
import { collection, projectOf } from "@/lib/schema";
import { useCan } from "@/lib/iam";
import { go } from "@/app/router";
import { collectionChanged } from "@/hooks/useCollection";
import { Form } from "./Form";

const time = (value: unknown) => {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) return "—";
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? "—" : date.toISOString().slice(0, 16).replace("T", " ") + " UTC";
};
const shortName = (value: unknown) => typeof value === "string" ? value.split("/").pop() || "—" : "—";

export function Backup({ r }: { r: Resource }) {
  const can = useCan();
  const [restoring, setRestoring] = useState(false);
  const project = projectOf(nameOf(r));
  const size = r.status?.sizeGib;
  const taken = r.status?.taken === true;
  const deleting = !!r.meta.deletedAt;
  const damaged = !!r.status?.verifyError;
  const restorable = taken && !deleting && !damaged && typeof size === "number" && Number.isFinite(size) && size > 0;
  const integrity = damaged ? "Check failed" : r.status?.verifiedAt ? "Verified" : "Not verified";
  const facts = [
    ["Source volume", shortName(r.spec?.volume)],
    ["Destination", shortName(r.spec?.target)],
    ["Volume size", taken && typeof size === "number" && size > 0 ? `${size.toLocaleString()} GiB` : "Not available yet"],
    ["Stored data", taken ? bytes(r.status?.storedBytes) : "Not available yet"],
    ["Created", time(r.status?.takenAt)],
    ["Integrity", integrity],
  ];
  return <div className="grid gap-4">
    <dl aria-label="Backup details" className="grid grid-cols-2 gap-x-6 gap-y-3 text-sm">
      {facts.map(([label, value]) => <div key={label} className="min-w-0">
        <dt className="text-xs text-muted-foreground">{label}</dt>
        <dd className="mt-1 break-words font-medium tabular-nums">{value}</dd>
      </div>)}
    </dl>
    {damaged && <p role="alert" className="text-sm text-destructive">This backup failed its integrity check. Choose another backup to restore.</p>}
    {project && can("write", "volumes", project) && <div>
      {!restoring && <Button size="sm" variant="secondary" disabled={!restorable} onClick={() => setRestoring(true)}>Restore volume</Button>}
      {restoring && restorable && <section aria-label="Restore volume" className="grid gap-3 border-t pt-4">
        <h3 className="text-sm font-medium">Restore as a new volume</h3>
        <Form coll={collection("volumes")!} initialProject={project} defaults={{ sourceBackup: nameOf(r), sizeGib: size }}
          onCancel={() => setRestoring(false)} onDone={(volume) => {
            collectionChanged("volumes");
            toast("Volume restore queued.");
            go({ view: "board", coll: "volumes", id: `${project}/${idOf(volume)}` });
          }} />
      </section>}
    </div>}
  </div>;
}
