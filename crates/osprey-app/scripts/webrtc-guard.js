// Switches off WebRTC before page scripts run.
//
// WebRTC can reveal your real IP address even when everything else goes
// through a proxy, because it talks to other computers directly over UDP.
// BM Osprey adds this to every tab while a route that hides your address
// is on (WARP, VPN, Tor), and always to private tabs. Video calls do not
// work while it is on. On Windows the engine is also told never to send
// WebRTC traffic outside the proxy.
(function () {
  'use strict';
  var names = ['RTCPeerConnection', 'webkitRTCPeerConnection', 'RTCDataChannel', 'RTCSessionDescription', 'RTCIceCandidate'];
  names.forEach(function (n) {
    try {
      Object.defineProperty(window, n, { value: undefined, writable: false, configurable: false });
    } catch (e) {}
  });
})();
