// Only exists because a service worker cannot touch the clipboard. Chromium
// ignores a copy of nothing, so the password is overwritten with a single space.
document.getElementById('blank').select();
document.execCommand('copy');
chrome.runtime.sendMessage({ type: 'clipboard-cleared' });
