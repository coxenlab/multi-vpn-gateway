//! netstat 只读快照。只把指定设备、未作用域化的路由当作候选所有权证据。
use std::collections::HashSet;
pub type Snapshot = HashSet<(String, bool, bool)>; // canonical CIDR, IPv6, reject

pub fn canonical(text: &str, v6: bool, host: bool) -> Option<String> {
    let (address, prefix) = text.split_once('/').map(|(a,p)| (a,Some(p))).unwrap_or((text,None));
    if address == "default" { return Some(if v6 {"::/0"} else {"0.0.0.0/0"}.into()); }
    if v6 {
        let ip: std::net::Ipv6Addr = address.split('%').next()?.parse().ok()?;
        let prefix: u32 = prefix.unwrap_or("128").parse().ok()?;
        if prefix > 128 { return None; }
        let mask = if prefix == 0 { 0 } else { u128::MAX << (128-prefix) };
        Some(format!("{}/{prefix}", std::net::Ipv6Addr::from(u128::from(ip)&mask)))
    } else {
        let parts: Vec<_> = address.split('.').collect();
        if parts.is_empty() || parts.len()>4 { return None; }
        let mut octets = [0u8;4];
        for (i,p) in parts.iter().enumerate() { octets[i] = p.parse().ok()?; }
        let prefix: u32 = match prefix { Some(p) => p.parse().ok()?, None => if host {32} else {parts.len() as u32*8} };
        if prefix > 32 { return None; }
        let mask = if prefix==0 {0} else {u32::MAX << (32-prefix)};
        Some(format!("{}/{prefix}",std::net::Ipv4Addr::from(u32::from_be_bytes(octets)&mask)))
    }
}

pub fn parse(text: &str, v6: bool, device: &str) -> Snapshot {
    let mut out=Snapshot::new(); let mut netif=None;
    for line in text.lines() {
        let columns: Vec<_> = line.split_whitespace().collect();
        if columns.first()==Some(&"Destination") { netif=columns.iter().position(|c| *c=="Netif"); continue; }
        let Some(i)=netif else {continue};
        if columns.len()<=i || columns.len()<3 || columns[i]!=device || columns[2].contains('I') {continue;}
        if let Some(cidr)=canonical(columns[0],v6,columns[2].contains('H')) {out.insert((cidr,v6,columns[2].contains('R')));}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_distinguishes_our_forward_reject_and_foreign_scoped_routes() {
        let text="Routing tables\nInternet:\nDestination Gateway Flags Netif Expire\n10 link#9 US utun226\n192.0.2/24 link#9 URS utun226\n192.0.2.8 link#9 UHS utun226\n172.16/12 link#4 US en0\n198.18/15 link#9 USI utun226\n";
        let s=parse(text,false,"utun226");
        assert_eq!(s.len(),3);
        assert!(s.contains(&("10.0.0.0/8".into(),false,false)));
        assert!(s.contains(&("192.0.2.0/24".into(),false,true)));
        assert!(s.contains(&("192.0.2.8/32".into(),false,false)));
        assert_eq!(canonical("10.3.4.5/16",false,false).as_deref(),Some("10.3.0.0/16"));
        assert_eq!(canonical("fd00:1::2/64",true,false).as_deref(),Some("fd00:1::/64"));
        assert_eq!(canonical("default",true,false).as_deref(),Some("::/0"));
        assert!(parse("Destination Gateway Flags Refs Use Mtu Netif Expire\nfd00::/8 link#9 URS 0 0 1500 utun226\n",true,"utun226").contains(&("fd00::/8".into(),true,true)));
    }
}
