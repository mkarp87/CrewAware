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
        r=pg.evaluate("(([x,y])=>{const r=document.getElementById('map').getBoundingClientRect();return [r.left+view.x+x*view.k, r.top+view.y+y*view.k]})",[px,py]); return r
    def click_px(px,py):
        x,y=scr(px,py); pg.mouse.click(x,y); pg.wait_for_timeout(150)
    pg.click("#tabs >> text=Setup")
    if pg.locator("#lu").count():
        pg.fill("#lu","admin"); pg.fill("#lp","adminpass1234"); pg.click("#btnLogin"); pg.wait_for_selector("text=Signed in as admin")
    pg.click(".subtabs >> text=Obstacles"); pg.click("text=Draw wall")
    pg.fill("#drawName","Fence")
    check(pg.locator("#drawFinish").is_disabled(),"finish disabled with 0 points")
    click_px(410,5); click_px(410,650)
    pg.click("#drawFinish"); pg.wait_for_timeout(600)
    s=state(); check(len(s['obstacles'])==1 and s['obstacles'][0]['kind']=="wall","wall saved")
    check(pg.locator("#map polyline").count()>=1,"wall drawn on map")
    # incident east of the wall, near the top
    pg.click("#btnPlace"); click_px(450,300); pg.wait_for_selector("text=Nearest available")
    rows=pg.locator("#body label.row").all_inner_texts()
    for r in rows: print(r.replace("\n"," | "))
    s=state(); inc=s['incidents'][0]; print("incident",inc['label'])
    cands=json.load(urllib.request.urlopen(f"{BASE}/api/nearest?lat={inc['lat']}&lon={inc['lon']}"))
    for c in cands: print(c['name'],round(c['straight_m']),round(c['dist_m']),round(c['eta_sec']),c['routed'])
    byname={c['name']:c for c in cands}
    check(byname['UTV 3']['dist_m']>byname['UTV 3']['straight_m']*1.5,"west-side unit detours around the wall")
    check(cands[0]['name'] in ('UTV 1','Cart 2'),"east-side unit ranks first: "+cands[0]['name'])
    check(any("by route" in r for r in rows),"UI says by route")
    # hover shows route line
    pg.locator("#body label.row").nth(len(rows)-1).hover(); pg.wait_for_timeout(300)
    check(pg.locator("#map polyline[stroke-dasharray]").count()>=1,"route polyline drawn on hover")
    pg.screenshot(path="/tmp/e2e/s3.png")
    # dispatch the first (pre-ticked)
    pg.click("#btnDispatch"); pg.wait_for_selector("text=Assigned")
    s=state(); d=[u for u in s['units'] if u['status']=='dispatched']; check(len(d)==1,"dispatched "+d[0]['name'])
    pg.click(".row:has-text('%s') button:has-text('On scene')"%d[0]['name']); pg.wait_for_timeout(500)
    s=state(); check([u for u in s['units'] if u['id']==d[0]['id']][0]['status']=="on_scene","on scene")
    pg.click("text=Close incident"); pg.wait_for_timeout(600)
    s=state(); check(not s['incidents'] and s['closed'],"incident closed and in recent list")
    check(all(u['status']=='available' for u in s['units']),"units released")
    print("log:",[e['text'] for e in s['log'][:6]])
    print("errors",errs); b.close()
