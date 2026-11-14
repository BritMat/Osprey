// Says in the title whether the wall is still there.
addEventListener('load', () => setTimeout(() => {
  document.title = document.getElementById('abnotify') ? 'wall still there' : 'wall removed';
}, 1000));
