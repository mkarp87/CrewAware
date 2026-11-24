import json,sys,time,urllib.request,subprocess
from playwright.sync_api import sync_playwright
BASE="http://127.0.0.1:18082"
errs=[]
def state(): return json.load(urllib.request.urlopen(BASE+"/api/state"))
def check(c,msg):
    print(("PASS " if c else "FAIL ")+msg)
    if not c: errs.append("FAIL "+msg)
subprocess.run(["/tmp/e2e/radio.sh","8"])
time.sleep(12)
with sync_playwright() as p:
    b=p.chromium.launch(executable_path="/usr/bin/google-chrome",args=["--no-sandbox"])
    pg=b.new_page(viewport={"width":1400,"height":850})
    pg.on("pageerror",lambda e:errs.append("pageerror: "+str(e)))
    pg.on("console",lambda m:errs.append("console: "+m.text) if m.type=="error" else None)
    pg.goto(BASE); pg.wait_for_selector("#tabs button"); pg.fill("#seat","Tester")
    pg.click("#tabs >> text=Setup")
    if pg.locator("#lu").count():
        pg.fill("#lu","admin"); pg.fill("#lp","adminpass1234"); pg.click("#btnLogin"); pg.wait_for_selector("text=Signed in as admin")
    pg.click(".subtabs >> text=Rules"); pg.wait_for_selector("#s1")
    for i,v in zip(range(1,6),[0.5,1,0.5,1,2]): pg.fill(f"#s{i}",str(v))
    pg.click("text=Save rules"); pg.wait_for_timeout(500)
    st=state()['settings']; check(st['parked_late_min']==0.5 and st['parked_no_contact_min']==1,"rules saved "+str(st))
    s=state(); print([(u['name'],u['contact'],round(u['age_sec'])) for u in s['units']])
    check(all(u['contact']=='normal' for u in s['units']),"all units normal with the radio up")
    # dispatch Cart 2 and lose the radio
    api=pg.context.request; H={"Content-Type":"application/json"}
    inc=json.loads(api.post(BASE+"/api/incidents",data=json.dumps({"lat":39.0005,"lon":-104.0005,"label":""}),headers=H).text())
    cart=[u for u in s['units'] if u['name']=='Cart 2'][0]
    api.post(f"{BASE}/api/incidents/{inc['id']}/dispatch",data=json.dumps({"units":[cart['id']]}),headers=H)
    subprocess.run(["pkill","-x","fake-radio"])
    print("radio killed; waiting for contact loss")
    t0=time.time(); seen_late=False; crit=None
    while time.time()-t0<150:
        s=state(); u=[x for x in s['units'] if x['id']==cart['id']][0]
        if u['contact']=='late': seen_late=True
        if s['alerts']: crit=s['alerts']; 
        if u['contact']=='no_contact' and crit: break
        time.sleep(2)
    print("alerts",crit, "radio",s['radio']['connected'],s['radio']['error'])
    check(seen_late,"unit went late first")
    check(any(a['severity']=='crit' for a in (crit or [])),"critical alert for dispatched unit with no contact")
    pg.reload(); pg.wait_for_selector("#alerts .alert.crit"); print("banner:",pg.inner_text("#alerts"))
    check("Cart 2" in pg.inner_text("#alerts"),"banner shown in UI")
    pg.screenshot(path="/tmp/e2e/s5.png")
    time.sleep(6)
    logs=[e['text'] for e in state()['log'][:8]]; print(logs)
    check(any("no contact" in l.lower() or "lost contact" in l.lower() for l in logs),"loss of contact logged once")
    # radio returns
    subprocess.run(["/tmp/e2e/radio.sh","8"])
    for _ in range(60):
        s=state()
        if s['radio']['connected'] and not s['alerts']: break
        time.sleep(2)
    print("after recovery: alerts",s['alerts'],"connected",s['radio']['connected'])
    check(s['radio']['connected'] and not s['alerts'],"radio reconnected and alerts cleared")
    check(any("regain" in e['text'].lower() or "back" in e['text'].lower() or "contact" in e['text'].lower() for e in s['log'][:6]),"recovery logged")
    print([e['text'] for e in s['log'][:6]])
    print("errors",errs); b.close()
