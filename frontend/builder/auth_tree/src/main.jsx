import { createRoot } from 'react-dom/client';
import TreeView from './components/TreeView.jsx';

const container = document.getElementById('root');
if (container) {
  const root = createRoot(container);
  root.render(<TreeView />);
}