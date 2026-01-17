const { invoke } = window.__TAURI__.core;
const { open } = window.__TAURI__.dialog;
const { listen } = window.__TAURI__.event;

// Open wiki button
document.getElementById('open-wiki').addEventListener('click', async () => {
  try {
    const file = await open({
      multiple: false,
      filters: [{
        name: 'TiddlyWiki',
        extensions: ['html', 'htm']
      }]
    });

    if (file) {
      await invoke('open_wiki_window', { path: file });
      loadRecentFiles();
    }
  } catch (err) {
    console.error('Error opening dialog:', err);
  }
});

// Drag and drop support
const dropZone = document.getElementById('drop-zone');

// Listen for Tauri drag-drop events
listen('tauri://drag-drop', async (event) => {
  const paths = event.payload.paths;
  if (paths && paths.length > 0) {
    for (const path of paths) {
      if (path.endsWith('.html') || path.endsWith('.htm')) {
        await invoke('open_wiki_window', { path });
        loadRecentFiles();
      }
    }
  }
});

listen('tauri://drag-enter', () => {
  dropZone.classList.add('drag-over');
});

listen('tauri://drag-leave', () => {
  dropZone.classList.remove('drag-over');
});

listen('tauri://drag-over', () => {
  dropZone.classList.add('drag-over');
});

// Also handle standard drag events for visual feedback
dropZone.addEventListener('dragover', (e) => {
  e.preventDefault();
  dropZone.classList.add('drag-over');
});

dropZone.addEventListener('dragleave', () => {
  dropZone.classList.remove('drag-over');
});

dropZone.addEventListener('drop', (e) => {
  e.preventDefault();
  dropZone.classList.remove('drag-over');
});

// Load and display recent files
async function loadRecentFiles() {
  try {
    const recent = await invoke('get_recent_files');
    const recentSection = document.getElementById('recent-section');
    const recentList = document.getElementById('recent-list');

    if (recent && recent.length > 0) {
      recentSection.style.display = 'block';
      recentList.innerHTML = '';

      for (const path of recent) {
        const li = document.createElement('li');
        const filename = path.split('/').pop().split('\\').pop();
        const dir = path.substring(0, path.length - filename.length - 1);

        li.innerHTML = `<span class="filename">${filename}</span><br><span class="path">${dir}</span>`;
        li.addEventListener('click', async () => {
          await invoke('open_wiki_window', { path });
          loadRecentFiles();
        });
        recentList.appendChild(li);
      }
    } else {
      recentSection.style.display = 'none';
    }
  } catch (err) {
    console.error('Error loading recent files:', err);
  }
}

// Load recent files on startup
loadRecentFiles();
