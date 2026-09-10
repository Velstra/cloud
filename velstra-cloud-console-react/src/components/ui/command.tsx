// The palette, over cmdk directly: the base-nova shadcn set has no command
// component, and cmdk is the primitive it would have wrapped anyway.

import { Command as Cmdk } from "cmdk";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";

export function CommandDialog({ open, onOpenChange, title, description, children }: {
  open: boolean; onOpenChange: (o: boolean) => void; title: string; description: string; children: React.ReactNode;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="overflow-hidden p-0" style={{ background: "var(--surface-raised)", borderColor: "var(--border-strong)" }}>
        <DialogTitle className="sr-only">{title}</DialogTitle>
        <DialogDescription className="sr-only">{description}</DialogDescription>
        <Cmdk label={title} className="flex flex-col">{children}</Cmdk>
      </DialogContent>
    </Dialog>
  );
}

export function CommandInput(props: React.ComponentProps<typeof Cmdk.Input>) {
  return (
    <div className="border-b px-3" style={{ borderColor: "var(--border-subtle)" }}>
      <Cmdk.Input {...props} className="h-11 w-full bg-transparent text-sm outline-none placeholder:text-[var(--text-faint)]" />
    </div>
  );
}
export const CommandList = (p: React.ComponentProps<typeof Cmdk.List>) =>
  <Cmdk.List {...p} className="max-h-[22rem] overflow-y-auto p-1" />;
export const CommandEmpty = (p: React.ComponentProps<typeof Cmdk.Empty>) =>
  <Cmdk.Empty {...p} className="px-3 py-6 text-center text-sm" style={{ color: "var(--text-muted)" }} />;
export const CommandGroup = (p: React.ComponentProps<typeof Cmdk.Group>) =>
  <Cmdk.Group {...p} className="px-1 py-1 [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1 [&_[cmdk-group-heading]]:text-[11px] [&_[cmdk-group-heading]]:font-semibold [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-[0.07em] [&_[cmdk-group-heading]]:text-[var(--text-muted)]" />;
export const CommandSeparator = (p: React.ComponentProps<typeof Cmdk.Separator>) =>
  <Cmdk.Separator {...p} className="my-1 h-px" style={{ background: "var(--border-subtle)" }} />;
export const CommandItem = (p: React.ComponentProps<typeof Cmdk.Item>) =>
  <Cmdk.Item {...p} className="flex cursor-pointer select-none items-center gap-2 rounded-[4px] px-2 py-1.5 text-sm data-[selected=true]:bg-[var(--surface-hover)] data-[selected=true]:text-[var(--text-strong)]" />;
