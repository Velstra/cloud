// noVNC ships no types. This is the sliver of its RFB class this console uses.
declare module "@novnc/novnc" {
  export default class RFB {
    constructor(target: HTMLElement, url: string, options?: { wsProtocols?: string[]; credentials?: Record<string, string> });
    disconnect(): void;
    sendCtrlAltDel(): void;
    focus(): void;
    addEventListener(kind: string, handler: (e: any) => void): void;
    scaleViewport: boolean;
    resizeSession: boolean;
  }
}
