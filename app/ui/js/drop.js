// 全局拖放：在 document 上拦下拖进来的文件，交给添加通道弹层。
// 页面没拦住时，外壳的导航策略会取消到 file:// 的导航，不会打开本地文件（DESIGN 7.5）。
// 只处理带文件的拖放；拖文字进输入框照常。顶层不碰 document / window。

export const hasFiles = dt => !!dt && Array.from(dt.types || []).includes('Files');

// onFiles(files: File[], {folders: 拖进来的文件夹个数})。返回卸载函数
export function installDrop(onFiles, doc = globalThis.document) {
  const over = e => {
    if (!hasFiles(e.dataTransfer)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  };
  const drop = e => {
    if (!hasFiles(e.dataTransfer)) return;
    e.preventDefault();
    const files = Array.from(e.dataTransfer.files || []);
    // 文件夹、多个文件、空文件都照样交出去，由添加通道弹层逐一提示。
    // 是不是文件夹只能在 drop 事件里同步读，事件结束后 items 就读不到了
    const folders = Array.from(e.dataTransfer.items || []).filter(it => {
      const entry = it.webkitGetAsEntry ? it.webkitGetAsEntry() : null;
      return !!(entry && entry.isDirectory);
    }).length;
    onFiles(files, { folders });
  };
  doc.addEventListener('dragover', over);
  doc.addEventListener('drop', drop);
  return () => {
    doc.removeEventListener('dragover', over);
    doc.removeEventListener('drop', drop);
  };
}
