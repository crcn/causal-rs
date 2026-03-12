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
      className="fixed inset-0 z-50 flex items-center justify-center"
      style={{ background: "rgba(0, 0, 0, 0.6)", backdropFilter: "blur(4px)" }}
      onClick={onClose}
    >
      <div
        className="relative w-[90vw] max-h-[90vh] overflow-auto rounded-xl border border-border p-5"
        style={{ background: "rgba(15, 15, 20, 0.95)", boxShadow: "0 24px 64px rgba(0, 0, 0, 0.5)" }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="absolute top-3 right-3 flex gap-1">
          <button
            onClick={async () => {
              await copyToClipboard(formatted);
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            }}
            className="p-1.5 rounded-md hover:bg-white/[0.05] transition-colors text-xs text-muted-foreground/50 hover:text-foreground"
            title="Copy payload"
          >
            {copied ? <Check size={14} /> : <Copy size={14} />}
          </button>
          <button
            onClick={onClose}
            className="p-1.5 rounded-md hover:bg-white/[0.05] transition-colors text-xs text-muted-foreground/50 hover:text-foreground"
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
      <pre className="p-2.5 text-[10px] rounded-md border border-border overflow-auto whitespace-pre-wrap resize-y min-h-24 max-h-[80vh]" style={{ background: "rgba(255, 255, 255, 0.015)" }}>
        <JsonSyntax json={formatted} />
      </pre>
      <div className="absolute top-2 right-2 z-10 flex gap-1">
        <button
          onClick={(e) => {
            e.stopPropagation();
            setModalOpen(true);
          }}
          className="p-1 rounded-md border border-border hover:bg-white/[0.05] transition-all text-[10px] text-muted-foreground/40 hover:text-muted-foreground"
          style={{ background: "rgba(10, 10, 15, 0.8)", backdropFilter: "blur(4px)" }}
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
          className="p-1 rounded-md border border-border hover:bg-white/[0.05] transition-all text-[10px] text-muted-foreground/40 hover:text-muted-foreground"
          style={{ background: "rgba(10, 10, 15, 0.8)", backdropFilter: "blur(4px)" }}
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
