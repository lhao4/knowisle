import { formatBytes, type NoteContent } from '../lib/ipc'

interface NoteViewerProps {
  note: NoteContent
}

/**
 * M0 的"编辑器"就是一个 `<pre>`。
 *
 * 这是刻意的：M0 要验证的是 **IPC 通道与文件读取是否正确**
 * （中文不乱码、BOM 已剥离、路径正确），不是编辑体验。
 * CodeMirror 在 M1 接入，届时这个组件会被整体替换。
 */
export function NoteViewer({ note }: NoteViewerProps) {
  const segments = note.path.split('/')

  return (
    <section className="viewer">
      <header className="viewer__header">
        <nav className="breadcrumb" aria-label="路径">
          {segments.map((segment, index) => (
            <span key={`${segment}-${index}`} className="breadcrumb__item">
              {index > 0 && <span className="breadcrumb__sep">/</span>}
              {segment}
            </span>
          ))}
        </nav>
        <span className="viewer__meta">
          {formatBytes(note.size)} · {new Date(note.mtime).toLocaleString('zh-CN')}
        </span>
      </header>

      <pre className="viewer__body">{note.content}</pre>
    </section>
  )
}
