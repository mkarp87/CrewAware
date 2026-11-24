import json,sys,time,urllib.request
from playwright.sync_api import sync_playwright
BASE="http://127.0.0.1:18082"
errs=[]
def state(): return json.load(urllib.request.urlopen(BASE+"/api/state"))
def check(c,msg):
    print(("PASS " if c else "FAIL ")+msg)
    if not c: errs.append("FAIL "+msg)
with sync_playwright() as p:
    b=p.chromium.launch(executable_path="/usr/bin/google-chrome",args=["--no-sandbox"])
    pg=b.new_page(viewport={"width":1400,"height":850})
    pg.on("pageerror",lambda e:errs.append("pageerror: "+str(e)))
    pg.on("console",lambda m:errs.append("console: "+m.text) if m.type=="error" else None)
    pg.goto(BASE); pg.wait_for_selector("#tabs button"); pg.fill("#seat","Tester")
    def scr(px,py):
        return pg.evaluate("(([x,y])=>{const r=document.getElementById('map').getBoundingClientRect();return [r.left+view.x+x*view.k, r.top+view.y+y*view.k]})",[px,py])
    def click_px(px,py):
        x,y=scr(px,py); pg.mouse.click(x,y); pg.wait_for_timeout(150)
    pg.click("#tabs >> text=Setup")
    if pg.locator("#lu").count():
        pg.fill("#lu","admin"); pg.fill("#lp","adminpass1234"); pg.click("#btnLogin"); pg.wait_for_selector("text=Signed in as admin")
    api=pg.context.request
    def nearest(inc):
        return {c['name']:c for c in json.loads(api.get(f"{BASE}/api/nearest?lat={inc['lat']}&lon={inc['lon']}").text())}
    # gate across the gap under the wall
    pg.click(".subtabs >> text=Obstacles"); pg.click("text=Draw gate"); pg.fill("#drawName","South gate")
    click_px(410,650); click_px(410,700); pg.click("#drawFinish"); pg.wait_for_timeout(500)
    pg.click("text=Draw slow zone"); pg.fill("#drawName","Sand"); pg.fill("#drawFactor","40")
    for q in [(600,200),(800,200),(800,400),(600,400)]: click_px(*q)
    pg.click("#drawFinish"); pg.wait_for_timeout(500)
    pg.click("text=Draw no-go area"); pg.fill("#drawName","Pond")
    for q in [(100,100),(200,100),(200,200),(100,200)]: click_px(*q)
    pg.click("#drawFinish"); pg.wait_for_timeout(500)
    s=state(); kinds=sorted(o['kind'] for o in s['obstacles']); print(kinds)
    check(kinds==['gate','no_go','slow','wall'] or kinds==['gate','no_go','slow','wall','wall'],"four obstacle kinds saved")
    gate=[o for o in s['obstacles'] if o['kind']=='gate'][0]; check(gate['open']==False,"gate starts closed")
    pg.screenshot(path="/tmp/e2e/s4a.png")
    # incident east, gate closed -> west units unroutable or far; open gate -> detour through gate
    pg.click("#tabs >> text=Units"); pg.click("#btnPlace"); click_px(450,300); pg.wait_for_selector("text=Nearest available")
    inc=state()['incidents'][0]
    n=nearest(inc); print({k:(round(v['dist_m']),v['routed']) for k,v in n.items()})
    closed3=n['UTV 3']['dist_m']
    # dispatcher opens the gate from the Units tab
    pg.click("#tabs >> text=Units"); pg.click("text=Open gate"); pg.wait_for_timeout(600)
    n=nearest(inc); check(n['UTV 3']['routed'] and n['UTV 3']['dist_m']<closed3*0.8,"open gate shortens UTV 3's route (%d m -> %d m)"%(closed3,n['UTV 3']['dist_m']))
    # slow zone: unit crossing it costs more than its distance
    print({k:(round(v['dist_m']),round(v['eta_sec'])) for k,v in n.items()})
    # reports
    r=api.get(f"{BASE}/api/incidents/{inc['id']}/report"); check(r.status==200 and "Incident" in r.text(),"printable report served")
    r=api.get(f"{BASE}/api/reports/incidents.csv"); print(r.text()[:200]); check(r.status==200,"csv served")
    # close incident
    pg.click("#tabs >> text=Incident"); pg.click("text=Close incident"); pg.wait_for_timeout(500)
    # profiles: save, export, import, activate
    pg.click("#tabs >> text=Setup"); pg.click(".subtabs >> text=Site"); pg.wait_for_selector("#btnSaveProfile")
    pg.click("#btnSaveProfile"); pg.wait_for_timeout(700)
    prof=json.loads(api.get(BASE+"/api/admin/profiles").text())['profiles']; print(prof)
    check(len(prof)==1 and prof[0]['active'],"profile saved and active")
    exp=api.get(f"{BASE}/api/admin/profiles/{prof[0]['id']}/export"); data=json.loads(exp.text()); check(data.get('format')=="crewaware-site/1","export format "+str(data.get('format')))
    open("/tmp/e2e/site.json","w").write(exp.text())
    pg.set_input_files("#importFile","/tmp/e2e/site.json"); pg.wait_for_timeout(1200)
    prof=json.loads(api.get(BASE+"/api/admin/profiles").text())['profiles']; print([(x['name'],x['active']) for x in prof])
    check(len(prof)==2,"import created a second profile")
    other=[x for x in prof if not x['active']][0]
    # opening an incident blocks switching
    inc2=json.loads(api.post(BASE+"/api/incidents",data=json.dumps({"lat":39.0,"lon":-104.0,"label":""}),headers={"Content-Type":"application/json"}).text())
    r=api.post(f"{BASE}/api/admin/profiles/{other['id']}/activate",data="{}",headers={"Content-Type":"application/json"}); print(r.status,r.text()); check(r.status>=400,"switch refused while an incident is open")
    api.post(f"{BASE}/api/incidents/{inc2['id']}/close",data="{}",headers={"Content-Type":"application/json"})
    pg.reload(); pg.wait_for_selector("#tabs button"); pg.click("#tabs >> text=Setup"); pg.wait_for_selector("text=Saved sites")
    pg.click(f"#body .row:has-text('{other['name']}') button:has-text('Switch')"); pg.click(f"#body .row:has-text('{other['name']}') button:has-text('Confirm switch')"); pg.wait_for_timeout(1000)
    s=state(); print(s['site']['name'],len(s['obstacles']),[ (r['name'],r['placed']) for r in s['repeaters']],len(s['units']))
    check(s['site']['name']==other['name'] and len(s['obstacles'])>=4,"switched to imported site with obstacles")
    check(all(r['placed'] for r in s['repeaters']) and len(s['units'])==6,"repeater placements restored, crew kept")
    pg.screenshot(path="/tmp/e2e/s4b.png")
    print("errors",errs); b.close()
