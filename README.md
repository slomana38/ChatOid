# ChatOid 0.1

ChatOid, insanların ve yerel Codex ajanlarının bire bir ve grup sohbeti yapması için iki masaüstü istemcisi sunar. Bire bir sohbetin hostu isteği açan cihazdır. Grubun hostu grup yöneticisinin cihazıdır. Host, sohbet geçmişini SQLite’ta tutar. Cihazlar aynı yerel ağ veya VPN üzerinde HTTPS ile bağlanır.

## Başlangıç

1. Windows’ta `chatoid.exe`, Linux’ta `chatoid` ile insan uygulamasını aç. **Hesap oluştur** bölümünde benzersiz bir kullanıcı adı, görünen ad ve en az 10 karakterlik bir parola seç.
2. Sohbet başlatacak cihazda **Host** sayfasını aç. Yerel ağ veya VPN IP adresini ve boş bir portu gir. Örnek `192.168.1.20:47821` veya `100.78.12.9:47821`. Hostu başlat. Ağdaki diğer cihazların bu porta erişebilmesi gerekir. Varsayılan `127.0.0.1` adresi aynı cihazda deneme içindir.
3. Karşı taraf **Kişiler ve istekler → Kişi kartımı kopyala** ile kartını paylaşır. Kodu içe aktar, kişiyi kullanıcı adıyla bul ve **Arkadaşlık isteği gönder** seçeneğini kullan. Panoya kopyalanan davet kodunu karşı tarafa ilet.
4. Alıcı kodu **Kişiler ve istekler** ekranında içe aktarır ve kabul eder. Bire bir sohbet iki tarafta da **Sohbetler** listesine eklenir. Aynı kullanıcı hesabı başka bir hosttan gelen daveti de açabilir.
5. **Yeni grup** ile kendi cihazında bir grup kur. Grubun **Davet et** menüsünde arkadaşlığını kabul eden kişileri seç. Davet bire bir sohbete düşer. Panodaki davet kodunu da paylaşabilirsin. Davetli kendi kabulünden sonraki mesajları görür.

Bir mesaja **Yanıtla** ile alıntı ekleyebilirsin. Alıntıya tıklamak kaynak mesaja gider. Arşivlenen sohbet yeni mesaj geldiğinde arşivde kalır. Host çevrimdışıyken sıraya alınan mesajlar **Bekliyor** görünür ve bağlantı geri geldiğinde yeniden gönderilir.

Host açıkken pencereyi kapatma düğmesi uygulamayı küçültür. **Hostu durdur**, **Oturumu kapat** ve **Uygulamadan çık** hostu durdurur. Host durduğunda uzaktaki üyeler önbellekteki sohbeti okuyabilir. Adres değiştiğinde aynı hostu yeni adresle başlat ve **Host bağlantısını kopyala** ile yeni kodu mevcut üyelere paylaş. Üyeler kodu **Kişiler ve istekler** ekranında içe aktarabilir.

## Ajan kurma

1. Ajan cihazında Codex CLI ile oturum aç. `chatoid-agent.exe` veya `chatoid-agent` ile ayrı ajan uygulamasını aç. **Ajan oluştur** ile ajana ait bir `.key` oluştur veya **Anahtarı aç** ile mevcut anahtarın yolunu gir.
2. **Ajan** sayfasında Codex’in çalıştırılabilir dosya yolunu gir. **Deneme yanıtı al** ile bağlantıyı kontrol et. **Yeni sohbet mesajlarını değerlendir** seçeneğini aç ve ayarları kaydet.
3. İnsan operatörünün kişi kartını **Operatörü eşleştir** alanına gir. Ajan bir istek alınca **Operatör bağlantısını kopyala** ile o isteğe özel yetki bağlantısı oluşur. Operatör bunu kendi insan uygulamasına aktarır ve isteği oradan kabul eder veya reddeder.
4. Otomatik karar için arkadaşlık veya grup iznini ajan ayarlarından aç ve karar kurallarını yaz. Ajan ile operatör aynı anda karar verirse host ilk kaydı korur.
5. Ajan üye olduktan sonra salt okunur bir klasör seç. **Çalışma klasörü** bölümünde hangi sohbetten hangi kişilerin dosya incelemesi isteyebileceğini işaretle ve kaydet. Grupta `@poyraz`, alıntılı yanıt veya «Poyraz, README.md dosyasını incele» gibi doğal bir hitap kullan.

İnsan parolası ve ajan özel anahtarı yerel cihazda kalır. Ajanın `.key` dosyası hesabın anahtarıdır. Sadece imzalı kişi kartını paylaş. Sohbet kayıtları hostta saklanır ve hostu çalıştıran kişi bu kayıtlara erişebilir. Codex sağlayıcısına gönderilen istek, ilgili sohbetin sınırlı bağlamını ve izinli dosya alıntılarını içerebilir.

## Derleme ve test

Rust 1.88 veya üstü ile yerel bir C derleyicisi gerekir. Windows için MSVC kullan. Ubuntu 24.04’te pencere ve OpenGL bağımlılıkları için `libx11-dev`, `libxcb1-dev`, `libxkbcommon-dev`, `libwayland-dev` ve `libgl1-mesa-dev` paketlerini kur.

```sh
cargo build --release --locked --bins
cargo fmt --check
cargo test --locked
cargo test --locked --test acceptance real_codex_natural_address_and_workspace_read -- --ignored
```

Son komut, oturum açılmış yerel Codex CLI ile gerçek model çağrısı yapar. Test verisi geçici bir klasörde oluşturulur. Pencereyi açmadan kimlik, HTTPS, ayrı grup onayı ve dosya incelemeli ajan yanıtını sınar.

Ayrı deneme profilleri için her iki istemci de `--data-dir KLASÖR` kabul eder. Uygulamalar bunun altında ayrı `human` ve `agent` dizinleri kullanır.

## Sürüm arşivleri

Python 3.11 veya üstüyle mevcut platformu ve kaynak dosyalarını paketle.

```sh
python tools/package.py windows
python tools/package.py linux
python tools/package.py source
python tools/package.py manifest
python tools/package.py verify
```

`windows` ve `linux` komutları, o platformda üretilen `target/release` dosyalarını kullanır. İki platformun arşivlerini aynı `dist` dizininde birleştirip SHA-256 listesini oluştur. GitHub Actions, `v0.1.0` gibi bir sürüm etiketi gönderildiğinde iki platformda derler, test eder ve doğrulanmış dağıtım çıktısını saklar.
