import { useState } from 'react'
import Canvas from './components/Canvas'
import Sidebar from './components/Sidebar'
import Header from './components/Header'
import { useWebSocket } from './hooks/useWebSocket'
import './App.css'

function App() {
  const [selectedWorkflow, setSelectedWorkflow] = useState(null)
  const { entries, stats, connected } = useWebSocket()

  function handleOrderCreated(data) {
    console.log('Order created successfully:', data)
    // Optionally auto-select the new workflow after a delay
    setTimeout(() => {
      // The correlation_id will match the order_id for new workflows
      // Uncomment to auto-select: setSelectedWorkflow(data.order_id)
    }, 1000)
  }

  return (
    <div className="h-screen flex flex-col overflow-hidden">
      <Header connected={connected} onCreateOrder={handleOrderCreated} />
      <div className="flex-1 flex overflow-hidden">
        <Canvas
          selectedWorkflow={selectedWorkflow}
          entries={entries}
        />
        <Sidebar
          entries={entries}
          stats={stats}
          selectedWorkflow={selectedWorkflow}
          onSelectWorkflow={setSelectedWorkflow}
        />
      </div>
    </div>
  )
}

export default App
