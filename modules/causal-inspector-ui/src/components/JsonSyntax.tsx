type JsonToken = { text: string; color: string };

function tokenizeJson(json: string): JsonToken[] {
  const tokens: JsonToken[] = [];
  const re =
    /("(?:[^"\\]|\\.)*")\s*:|("(?:[^"\\]|\\.)*")|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|(\btrue\b|\bfalse\b)|(\bnull\b)|([{}[\]:,])/g;
  let lastIndex = 0;
  let match: RegExpExecArray | null;

  while ((match = re.exec(json)) !== null) {
    if (match.index > lastIndex) {
      tokens.push({ text: json.slice(lastIndex, match.index), color: "" });
    }
    if (match[1] !== undefined) {
      tokens.push({ text: match[1], color: "text-blue-400" });
      tokens.push({ text: ":", color: "text-zinc-500" });
    } else if (match[2] !== undefined) {
      tokens.push({ text: match[2], color: "text-green-400" });
    } else if (match[3] !== undefined) {
      tokens.push({ text: match[3], color: "text-amber-400" });
    } else if (match[4] !== undefined) {
      tokens.push({ text: match[4], color: "text-purple-400" });
    } else if (match[5] !== undefined) {
      tokens.push({ text: match[5], color: "text-zinc-500" });
    } else if (match[6] !== undefined) {
      tokens.push({ text: match[6], color: "text-zinc-500" });
    }
    lastIndex = re.lastIndex;
  }
  if (lastIndex < json.length) {
    tokens.push({ text: json.slice(lastIndex), color: "" });
  }
  return tokens;
}

export function JsonSyntax({ json }: { json: string }) {
  const tokens = tokenizeJson(json);
  return (
    <>
      {tokens.map((t, i) => (
        <span key={i} className={t.color}>
          {t.text}
        </span>
      ))}
    </>
  );
}
