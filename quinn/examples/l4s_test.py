#!/usr/bin/env python3

"""
linuxrouter.py: Example network with Linux IP router

This example converts a Node into a router using IP forwarding already built
into Linux.

The example topology creates a router and two IP subnets:

    - 192.168.1.0/24 (r0-eth1, IP: 192.168.1.1)
    - 172.16.0.0/24 (r0-eth2, IP: 172.16.0.1)

Each subnet consists of one or more host connected to a single switch:

    r0-eth1 - s1-eth1 - h1-eth0 (IP: 192.168.1.100)
    r0-eth2 - s2-eth1 - h2-eth0 (IP: 172.16.0.100)

The example relies on default routing entries that are automatically created
for each router interface, as well as 'defaultRoute' parameters for the host
interfaces.

Additional routes may be added to the router or hosts by executing 'ip route'
or 'route' commands on the router or hosts.
"""

from mininet.topo import Topo
from mininet.net import Mininet
from mininet.node import Node
from mininet.log import setLogLevel, info
from mininet.cli import CLI
from mininet.nodelib import LinuxBridge
from mininet.link import TCLink


class LinuxRouter(Node):
    "A Node with IP forwarding enabled."

    # pylint: disable=arguments-differ
    def config(self, **params):
        super(LinuxRouter, self).config(**params)

        # Enable forwarding on the router
        self.cmd('sysctl net.ipv4.ip_forward=1')

        ## Avoid processing 64K packets in the kernel, which will send those packets in a burst independent of the pacing (lro only for newer NICS and kernels that support it):
        #self.cmd('ethtool -K $NETIF tso off gso off gro off lro off')
        ## fq qdisc needs to be configured on clients and server NICS (instead of fq_codel; fq is the only one that supports the pacing)
        #self.cmd('tc qdisc replace dev $NETIF root handle 1: fq limit 20480 flow_limit 10240')
        ## Enable Accurate ECN (only needed for BBR2 and DCTCP, not needed for Prague)
        #self.cmd('sysctl net.ipv4.tcp_ecn=3')
        ## set Prague congestion control system wide (or in the application with socket options)
        #self.cmd('sysctl net.ipv4.tcp_congestion_control=prague')

    def terminate(self):
        self.cmd('sysctl net.ipv4.ip_forward=0')
        super(LinuxRouter, self).terminate()


class NetworkTopo(Topo):
    "A LinuxRouter connecting three IP subnets"

    # pylint: disable=arguments-differ
    def build(self, **_opts):

        defaultIP = '192.168.1.1/24'  # IP address for r0-eth1
        router = self.addNode('r0', cls=LinuxRouter, ip=defaultIP)

        s1, s2 = [self.addSwitch(s) for s in ('s1', 's2')]

        self.addLink(s1, router, cls=TCLink, bw=10,
                     intfName2='r0-eth1', params2={'ip': defaultIP})
        self.addLink(s2, router, cls=TCLink, bw=5,
                     intfName2='r0-eth2', params2={'ip': '172.16.0.1/24'})

        h1 = self.addHost('h1', ip='192.168.1.100/24',
                          defaultRoute='via 192.168.1.1')
        h2 = self.addHost('h2', ip='192.168.1.101/24',
                          defaultRoute='via 192.168.1.1')
        h3 = self.addHost('h3', ip='172.16.0.100/24',
                          defaultRoute='via 172.16.0.1')

        for h, s in [(h1, s1), (h2, s1), (h3, s2)]:
            self.addLink(h, s)


def run():
    "Test linux router"
    topo = NetworkTopo()
    net = Mininet(topo=topo, switch=LinuxBridge,
                  waitConnected=True)
    net.start()

    net['h3'].cmd('target/debug/perf_server --congestion prague &')
    net['h1'].cmd('target/debug/perf_client --congestion prague --duration 3 172.16.0.100:4433')
    net['h2'].cmd('target/debug/perf_client --congestion prague --duration 3 172.16.0.100:4433')

    info('*** Routing Table on Router:\n')
    info(net['r0'].cmd('route'))
    CLI(net)
    net.stop()


if __name__ == '__main__':
    setLogLevel('info')
    run()
