function countNodes(nodes) {
  if (!nodes || !Array.isArray(nodes)) return 0
  return nodes.reduce((sum, node) => sum + 1 + countNodes(node.children || []), 0)
}

function countEffects(nodes) {
  if (!nodes || !Array.isArray(nodes)) return 0
  return nodes.reduce((sum, node) => sum + (node.effects?.length || 0) + countEffects(node.children || []), 0)
}

function TreeNode({ node }) {
  const time = new Date(node.created_at).toLocaleTimeString()

  return (
    <div className="ml-8 border-l-2 border-slate-700 pl-6 mt-4 relative before:absolute before:left-0 before:top-6 before:w-6 before:h-0.5 before:bg-slate-700">
      <div className="flex items-center gap-3 p-4 bg-slate-950 border border-slate-700 rounded-lg mb-3">
        <div className="w-8 h-8 rounded-lg bg-blue-500/10 text-blue-400 flex items-center justify-center text-lg flex-shrink-0">
          📤
        </div>
        <div className="flex-1 min-w-0">
          <div className="font-semibold text-slate-50 text-sm">{node.event_type}</div>
          <div className="text-xs text-slate-500 font-mono mt-0.5">
            id: {node.event_id.substring(0, 8)}...
          </div>
        </div>
        <div className="text-xs text-slate-500 whitespace-nowrap">{time}</div>
      </div>

      {node.effects && node.effects.length > 0 && (
        <div className="pl-8 space-y-2 mb-2">
          {node.effects.map((effect, idx) => (
            <div
              key={idx}
              className="flex items-center gap-3 p-3 bg-slate-900 border border-slate-700 rounded-lg text-sm"
            >
              <div
                className={`w-6 h-6 rounded flex items-center justify-center text-xs flex-shrink-0 ${
                  effect.status === 'completed'
                    ? 'bg-emerald-500/10 text-emerald-400'
                    : effect.status === 'failed'
                    ? 'bg-red-500/10 text-red-400'
                    : 'bg-purple-500/10 text-purple-400'
                }`}
              >
                {effect.status === 'completed' ? '✓' : effect.status === 'failed' ? '✗' : '⋯'}
              </div>
              <span className="flex-1 text-slate-200">{effect.effect_id}</span>
              {effect.attempts > 1 && (
                <span className="text-xs text-slate-500">{effect.attempts} attempts</span>
              )}
            </div>
          ))}
        </div>
      )}

      {node.children && node.children.map((child, idx) => (
        <TreeNode key={idx} node={child} />
      ))}
    </div>
  )
}

export default function TreeView({ tree, correlationId }) {
  if (!tree || !Array.isArray(tree) || tree.length === 0) {
    return (
      <div className="text-center py-12">
        <p className="text-slate-500">No tree data available</p>
      </div>
    )
  }

  const nodeCount = countNodes(tree)
  const effectCount = countEffects(tree)

  return (
    <div>
      <div className="bg-slate-900 border border-slate-700 rounded-lg p-6 mb-8">
        <h2 className="text-xl font-semibold text-slate-50 mb-4">Workflow Tree</h2>
        <div className="flex gap-6 text-sm">
          <div className="flex items-center gap-2">
            <span>📋</span>
            <span className="text-slate-400">ID: {correlationId.substring(0, 8)}...</span>
          </div>
          <div className="flex items-center gap-2">
            <span>🌳</span>
            <span className="text-slate-400">{nodeCount} events</span>
          </div>
          <div className="flex items-center gap-2">
            <span>⚙️</span>
            <span className="text-slate-400">{effectCount} effects</span>
          </div>
        </div>
      </div>

      <div className="bg-slate-900 border border-slate-700 rounded-lg p-8">
        {tree.map((node, idx) => (
          <TreeNode key={idx} node={node} />
        ))}
      </div>
    </div>
  )
}
