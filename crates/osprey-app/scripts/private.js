// Extra protection for private tabs, injected before page scripts,
// together with webrtc-guard.js. Camera and microphone are refused, so a
// private tab cannot be asked to identify you through them.
(function () {
  'use strict';
  try {
    if (navigator.mediaDevices) {
      Object.defineProperty(navigator.mediaDevices, 'getUserMedia', {
        value: function () { return Promise.reject(new DOMException('Not allowed in private tabs', 'NotAllowedError')); },
        configurable: false
      });
    }
  } catch (e) {}
})();
