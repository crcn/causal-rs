import { useState, useEffect } from "react";
import { createPortal } from "react-dom";
import { JsonSyntax } from "./JsonSyntax";
import { copyToClipboard } from "../utils";
import { Copy, Check, Maximize2, X } from "lucide-react";

function formatPayload(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

function PayloadModal({
  formatted,
  onClose,
}: {
  formatted: string;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handleKey);
    return () => document.removeEventListener("keydown", handleKey);
  }, [onClose]);

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/70"
      onClick={onClose}
    >
      <div
        className="relative w-[90vw] max-h-[90vh] overflow-auto rounded-lg border border-border bg-background p-4"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="absolute top-2 right-2 flex gap-1">
          <button
            onClick={async () => {
              await copyToClipboard(formatted);
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            }}
            className="p-1 rounded hover:bg-accent transition-colors text-xs"
            title="Copy payload"
          >
            {copied ? <Check size={14} /> : <Copy size={14} />}
          </button>
          <button
            onClick={onClose}
            className="p-1 rounded hover:bg-accent transition-colors text-xs text-muted-foreground"
            title="Close"
          >
            <X size={14} />
          </button>
        </div>
        <pre className="text-xs whitespace-pre-wrap">
          <JsonSyntax json={formatted} />
        </pre>
      </div>
    </div>,
    document.body
  );
}

export function CopyablePayload({
  payload,
  className = "",
}: {
  payload: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const [modalOpen, setModalOpen] = useState(false);
  const formatted = formatPayload(payload);

  return (
    <div className={`relative ${className}`}>
      <pre className="p-2 text-[10px] bg-background rounded border border-border overflow-auto whitespace-pre-wrap resize-y min-h-24 max-h-[80vh]">
        <JsonSyntax json={formatted} />
      </pre>
      <div className="absolute top-1.5 right-1.5 z-10 flex gap-1">
        <button
          onClick={(e) => {
            e.stopPropagation();
            setModalOpen(true);
          }}
          className="p-1 rounded bg-background/80 border border-border hover:bg-accent transition-colors text-[10px] text-muted-foreground"
          title="Expand"
        >
          <Maximize2 size={12} />
        </button>
        <button
          onClick={async (e) => {
            e.stopPropagation();
            await copyToClipboard(formatted);
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          }}
          className="p-1 rounded bg-background/80 border border-border hover:bg-accent transition-colors text-[10px] text-muted-foreground"
          title="Copy"
        >
          {copied ? <Check size={12} /> : <Copy size={12} />}
        </button>
      </div>
      {modalOpen && (
        <PayloadModal
          formatted={formatted}
          onClose={() => setModalOpen(false)}
        />
      )}
    </div>
  );
}
